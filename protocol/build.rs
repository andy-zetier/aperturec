use asn1rs::converter::Converter;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use syn::visit_mut::VisitMut;
use syn::*;

#[cfg(feature = "asn1c-tests")]
use std::process::Command;
#[cfg(feature = "asn1c-tests")]
use syn::visit::Visit;

fn create_dir_if_not_exists(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    } else if !path.is_dir() {
        anyhow::bail!("{} exists and is not a directory", path.display());
    }

    Ok(())
}

fn asn1_srcs() -> anyhow::Result<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = fs::read_dir("asn1")
        .unwrap()
        .filter_map(|de_res| de_res.ok())
        .filter(|de| de.path().extension().is_some_and(|ext| ext == "asn1"))
        .map(|de| de.path())
        .collect();

    Ok(paths)
}

fn common_attributes(should_impl_copy: bool) -> Vec<Attribute> {
    let copy: punctuated::Punctuated<PathSegment, Token![,]> = if should_impl_copy {
        parse_quote! { Copy }
    } else {
        parse_quote! {}
    };
    vec![
        parse_quote! { #[derive(rasn::AsnType, rasn::Encode, rasn::Decode, new, Debug, Clone, PartialEq, Eq, #copy)] },
        parse_quote! { #[rasn(automatic_tags, option_type(Option))] },
    ]
}

fn asn_attr_meta_list<'a, A>(curr_attrs: A) -> impl Iterator<Item = Meta> + 'a
where
    A: IntoIterator<Item = &'a Attribute>,
    <A as IntoIterator>::IntoIter: 'a,
{
    curr_attrs
        .into_iter()
        .filter(|attr| attr.style == AttrStyle::Outer)
        .filter_map(|attr| match &attr.meta {
            Meta::List(list) => Some(list),
            _ => None,
        })
        .filter(|list| list.path.is_ident("asn"))
        .map(|list| {
            list.parse_args_with(punctuated::Punctuated::<Meta, Token![,]>::parse_terminated)
                .unwrap()
        })
        .flat_map(|puncted| puncted.into_iter())
}

fn is_extensible<'a, A>(curr_attrs: A) -> bool
where
    A: IntoIterator<Item = &'a Attribute>,
    <A as IntoIterator>::IntoIter: 'a,
{
    let meta_list = asn_attr_meta_list(curr_attrs);
    for meta in meta_list {
        match meta {
            Meta::List(ml) => {
                if ml.path.is_ident("extensible_after") {
                    return true;
                }
            }
            _ => {}
        }
    }

    false
}

fn is_asn_enumerated<'a, A>(attrs: A) -> bool
where
    A: IntoIterator<Item = &'a Attribute>,
    <A as IntoIterator>::IntoIter: 'a,
{
    let meta_list = asn_attr_meta_list(attrs);
    for meta in meta_list {
        match meta {
            Meta::Path(path) => {
                if path.is_ident("enumerated") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

struct RasnDerive;
impl VisitMut for RasnDerive {
    fn visit_item_struct_mut(&mut self, i: &mut ItemStruct) {
        let is_extensible = is_extensible(&i.attrs);

        i.attrs = common_attributes(false);
        for field in &mut i.fields {
            field.attrs = vec![];
        }
        if let Fields::Unnamed(fields_unnamed) = &i.fields {
            if fields_unnamed.unnamed.len() == 1 {
                i.attrs.push(parse_quote! { #[rasn(delegate)] });
            }
        }
        if is_extensible {
            i.attrs.push(parse_quote! { #[non_exhaustive] });
            i.attrs.push(parse_quote! { #[derive(Builder)] });
        }
    }

    fn visit_item_enum_mut(&mut self, i: &mut ItemEnum) {
        let mut should_impl_copy = false;
        let is_extensible = is_extensible(&i.attrs);
        let mut rasn_attrs = vec![];
        if is_asn_enumerated(&i.attrs) {
            rasn_attrs.push(parse_quote! { #[rasn(enumerated)] });
            should_impl_copy = true;
        } else {
            rasn_attrs.push(parse_quote! { #[rasn(choice)] });
        }
        i.attrs = common_attributes(should_impl_copy);
        i.attrs.append(&mut rasn_attrs);
        for variant in &mut i.variants {
            variant.attrs = vec![];
        }
        if is_extensible {
            i.attrs.push(parse_quote! { #[non_exhaustive] });
        }
    }
}

fn has_asn1_variants_impl_items(items: &Vec<ImplItem>) -> bool {
    let (mut impl_variant, mut impl_variants, mut impl_value_index) = (false, false, false);
    for item in items {
        match item {
            ImplItem::Fn(impl_item_fn) => {
                if impl_item_fn.sig.ident == "variant" {
                    impl_variant = true;
                }
                if impl_item_fn.sig.ident == "variants" {
                    impl_variants = true;
                }
                if impl_item_fn.sig.ident == "value_index" {
                    impl_value_index = true;
                }
            }
            _ => continue,
        }
    }

    impl_variant && impl_variants && impl_value_index
}

struct ImplRemove;
impl VisitMut for ImplRemove {
    fn visit_file_mut(&mut self, i: &mut File) {
        i.items.retain(|item| match item {
            Item::Impl(ItemImpl { items, .. }) => has_asn1_variants_impl_items(&items),
            _ => true,
        });
    }
}

struct UseChanges;
impl VisitMut for UseChanges {
    fn visit_file_mut(&mut self, i: &mut File) {
        let target: Item = parse_quote! { use asn1rs::prelude::*; };
        i.items.retain(|item| item != &target);
        i.items.append(&mut vec![
            parse_quote! {
                #[allow(unused_imports)]
                use rasn::AsnType;
            },
            parse_quote! {
                #[allow(unused_imports)]
                use derive_builder::Builder;
            },
            parse_quote! {
                #[allow(unused_imports)]
                use derive_new::new;
            },
        ]);
    }
}

struct OctetStringFixup;
impl VisitMut for OctetStringFixup {
    fn visit_field_mut(&mut self, i: &mut Field) {
        let meta_list = asn_attr_meta_list(&i.attrs);
        let is_octet_str = meta_list
            .filter(|meta| match meta {
                Meta::Path(path) => path.is_ident("octet_string"),
                _ => false,
            })
            .next()
            .is_some();
        if is_octet_str {
            i.ty = parse_quote! { rasn::types::OctetString };
        }
    }
}

fn generate_rust_bindings(asn1_paths: &Vec<PathBuf>, out_dir: &Path) -> anyhow::Result<()> {
    let mut converter = Converter::default();

    let asn1rs_dir = out_dir.join("asn1rs");
    create_dir_if_not_exists(&asn1rs_dir)?;

    for path in asn1_paths {
        let path_display = path.display();
        if let Err(e) = converter.load_file(&path) {
            panic!("Loading of {} failed: {:?}", path_display, e)
        }
        converter.to_rust(&asn1rs_dir, |_| {}).unwrap();

        let rust_fname = PathBuf::from(path.file_stem().unwrap()).with_extension("rs");
        let asn1rs_generated = asn1rs_dir.join(&rust_fname);
        let mut parsed = syn::parse_file(&fs::read_to_string(&asn1rs_generated).unwrap()).unwrap();

        OctetStringFixup.visit_file_mut(&mut parsed);
        ImplRemove.visit_file_mut(&mut parsed);
        RasnDerive.visit_file_mut(&mut parsed);
        UseChanges.visit_file_mut(&mut parsed);

        let generated_path = out_dir.join(rust_fname);
        let generated = prettyplease::unparse(&parsed);

        fs::write(generated_path, generated).unwrap();
    }

    Ok(())
}

#[cfg(feature = "asn1c-tests")]
fn find_generated_sources_and_headers(out_dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut sources = Vec::new();
    let mut headers = Vec::new();

    if out_dir.is_dir() {
        for entry in fs::read_dir(out_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() {
                if let Some(extension) = path.extension() {
                    match extension.to_str().unwrap() {
                        "c" => sources.push(PathBuf::from(path)),
                        "h" => headers.push(PathBuf::from(path)),
                        _ => {}
                    }
                }
            }
        }
    }

    // Enforce consistent header order
    headers.sort_by_key(|path| {
        path.clone()
            .into_os_string()
            .into_string()
            .unwrap()
            .to_lowercase()
    });

    (sources, headers)
}

#[cfg(feature = "asn1c-tests")]
fn generate_asn1c_files<I>(
    asn1_srcs: I,
    out_dir: &Path,
) -> anyhow::Result<(Vec<PathBuf>, Vec<PathBuf>)>
where
    I: IntoIterator<Item = PathBuf>,
{
    let args = asn1_srcs
        .into_iter()
        .map(|path: PathBuf| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let status = Command::new("asn1c")
        .args(["-D", out_dir.to_str().unwrap()])
        .arg("-fcompound-names")
        .arg("-no-gen-example")
        .args(args)
        .status()?;
    if !status.success() {
        anyhow::bail!("Process failed with code {}", status.code().unwrap());
    }
    Ok(find_generated_sources_and_headers(out_dir))
}

struct TypesWithDescriptorCollector {
    inner: Vec<Ident>,
}
impl<'ast> visit::Visit<'ast> for TypesWithDescriptorCollector {
    fn visit_foreign_item_static(&mut self, i: &'ast ForeignItemStatic) {
        match &*i.ty {
            Type::Path(path) => {
                if let Some(ident) = path.path.get_ident() {
                    if ident == "asn_TYPE_descriptor_t" {
                        self.inner.push(Ident::new(
                            &i.ident.to_string()["as_DEF_".len() + 1..],
                            i.ident.span(),
                        ));
                    }
                }
            }
            _ => {}
        }
    }
}

struct StructFilter {
    unfiltered: Vec<Ident>,
    filtered: Vec<Ident>,
}

impl<'ast> visit::Visit<'ast> for StructFilter {
    fn visit_item_struct(&mut self, i: &'ast ItemStruct) {
        if self.unfiltered.contains(&i.ident) {
            self.filtered.push(i.ident.clone());
        }
    }
}

#[cfg(feature = "asn1c-tests")]
fn generate_asn1_traits<S>(bindings: S) -> anyhow::Result<String>
where
    S: AsRef<str>,
{
    let mut syntax = syn::parse_file(bindings.as_ref())?;
    let mut collector = TypesWithDescriptorCollector { inner: vec![] };
    collector.visit_file(&syntax);
    let mut filter = StructFilter {
        unfiltered: collector.inner,
        filtered: vec![],
    };
    filter.visit_file(&syntax);
    for ident in filter.filtered {
        let new_ident = Ident::new(&format!("asn_DEF_{}", ident), ident.span());
        syntax.items.push(parse_quote! {
            impl crate::test::c::ASN1GenType for #ident {
                unsafe fn get_descriptor() -> &'static asn_TYPE_descriptor_t {
                    &#new_ident
                }
            }
        });
    }

    Ok(prettyplease::unparse(&syntax))
}

#[cfg(feature = "asn1c-tests")]
fn generate_c_bindings<I>(asn1_srcs: I, out_dir: &Path) -> anyhow::Result<()>
where
    I: IntoIterator<Item = PathBuf>,
{
    let c_source_dir = out_dir.join("source");
    create_dir_if_not_exists(&c_source_dir)?;
    let c_build_dir = out_dir.join("build");
    create_dir_if_not_exists(&c_build_dir)?;
    let bindings_dir = out_dir.join("bindings");
    create_dir_if_not_exists(&bindings_dir)?;

    let (sources, headers) = generate_asn1c_files(asn1_srcs, &c_source_dir)?;

    let mut cc_builder = cc::Build::new();
    cc_builder.include(&c_source_dir);
    cc_builder.files(sources);
    cc_builder.flag("-Wno-missing-field-initializers");
    cc_builder.flag("-Wno-missing-braces");
    cc_builder.flag("-Wno-unused-parameter");
    cc_builder.flag("-Wno-unused-const-variable");
    cc_builder.out_dir(&c_build_dir);
    cc_builder.compile("ac_asn1c_codec");

    let mut builder = bindgen::Builder::default()
        .clang_arg(format!("-I{}", c_source_dir.display()))
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .derive_debug(true)
        .layout_tests(false);
    for header in headers {
        builder = builder.header(header.to_str().unwrap());
    }
    let bindings = builder.generate().unwrap();
    let bindings_with_trait_impls = generate_asn1_traits(bindings.to_string())?;
    fs::write(bindings_dir.join("bindings.rs"), bindings_with_trait_impls)?;

    Ok(())
}

fn main() -> anyhow::Result<()> {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let asn1_srcs = asn1_srcs()?;
    for path in &asn1_srcs {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    generate_rust_bindings(&asn1_srcs, &out_dir)?;

    #[cfg(feature = "asn1c-tests")]
    generate_c_bindings(asn1_srcs, &out_dir.join("c"))?;

    Ok(())
}
