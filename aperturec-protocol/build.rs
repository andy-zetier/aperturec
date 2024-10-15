use std::collections::HashMap;
use std::env;
use std::format;
use std::fs;
use std::path::PathBuf;
use syn::parse_quote;
use syn::visit_mut::VisitMut;

const RAW_DIR: &str = "rust_raw";

struct DeriveNew;
impl VisitMut for DeriveNew {
    fn visit_item_struct_mut(&mut self, node: &mut syn::ItemStruct) {
        node.attrs.push(parse_quote! { #[derive(derive_new::new)] });
    }
}

struct DeriveBuilder;
impl VisitMut for DeriveBuilder {
    fn visit_item_struct_mut(&mut self, node: &mut syn::ItemStruct) {
        node.attrs
            .push(parse_quote! { #[derive(derive_builder::Builder)] });
        node.attrs
            .push(parse_quote! { #[builder(default, setter(into, strip_option))] });
    }
}

#[derive(Default)]
struct DeriveFrom {
    wrapped_options: HashMap<syn::ItemStruct, (syn::Ident, syn::Type)>,
}

impl DeriveFrom {
    fn complete(self, file: &mut syn::File) {
        file.items.push(parse_quote! {
            /// Error when converting a struct which wraps a single Option to it's inner type, and
            /// the Option is None.
            #[derive(Debug)]
            pub struct WrappedOptionalConvertError;
        });

        file.items.push(parse_quote! {
            impl std::fmt::Display for WrappedOptionalConvertError {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
                    <Self as std::fmt::Debug>::fmt(self, f)
                }
            }
        });

        file.items.push(parse_quote! {
            impl std::error::Error for WrappedOptionalConvertError {}
        });

        for (item_struct, (field_ident, generic_ty)) in self.wrapped_options {
            let outer_ident = item_struct.ident;

            file.items.push(parse_quote! {
                impl TryFrom<#outer_ident> for #generic_ty {
                    type Error = WrappedOptionalConvertError;

                    fn try_from(v: #outer_ident) -> Result<#generic_ty, Self::Error> {
                        v.#field_ident.ok_or(WrappedOptionalConvertError)
                    }
                }
            });
        }
    }
}

impl VisitMut for DeriveFrom {
    fn visit_item_enum_mut(&mut self, node: &mut syn::ItemEnum) {
        node.attrs
            .push(parse_quote! { #[derive(derive_more::From)] });
        for variant in node.variants.iter_mut() {
            if variant.fields.len() != 1 {
                continue;
            }
            variant.attrs.push(parse_quote! { #[from] });
        }
    }

    fn visit_item_struct_mut(&mut self, node: &mut syn::ItemStruct) {
        node.attrs
            .push(parse_quote! { #[derive(derive_more::From)] });
        node.attrs.push(parse_quote! { #[from(forward)] });

        if node.fields.len() == 1 {
            let field = node.fields.iter().next().unwrap();
            if field.ident.is_none() {
                return;
            }
            let path = match &field.ty {
                syn::Type::Path(type_path) => &type_path.path,
                _ => return,
            };

            let last_segment = match path.segments.last() {
                Some(last) => last,
                _ => return,
            };
            if last_segment.ident != "Option" {
                return;
            }

            let generic_args = match &last_segment.arguments {
                syn::PathArguments::AngleBracketed(angle_bracketed) => &angle_bracketed.args,
                _ => panic!("Option with non Angle-Bracket parameterization"),
            };

            if generic_args.len() != 1 {
                panic!("Option with more than one generic argument");
            }
            let generic_ty = match generic_args.first().unwrap() {
                syn::GenericArgument::Type(ty) => ty,
                _ => panic!("Option parameterized with something other than a type"),
            };

            self.wrapped_options.insert(
                node.clone(),
                (field.ident.clone().unwrap(), generic_ty.clone()),
            );
        }
    }
}

fn main() -> anyhow::Result<()> {
    let proto_srcs: Vec<PathBuf> = glob::glob("proto/*.proto")?.collect::<Result<Vec<_>, _>>()?;
    for path in &proto_srcs {
        println!("cargo:rerun-if-changed={}", path.display());
    }

    let mut config = prost_build::Config::new();
    let outdir = PathBuf::from(env::var("OUT_DIR")?);
    let raw_outdir = outdir.join(RAW_DIR);

    fs::create_dir_all(&raw_outdir)?;
    config.out_dir(&raw_outdir);

    // Enable FragmentId type to be used similar to an integer type
    config.message_attribute("FragmentId", "#[derive(PartialOrd, Ord, Eq)]");
    config.compile_protos(&proto_srcs, &["proto/"])?;

    let raw_srcs: Vec<PathBuf> =
        glob::glob(&format!("{}/*.rs", raw_outdir.display()))?.collect::<Result<Vec<_>, _>>()?;

    for raw_src in &raw_srcs {
        let mut parsed = syn::parse_file(&fs::read_to_string(raw_src)?)?;

        DeriveNew.visit_file_mut(&mut parsed);
        DeriveBuilder.visit_file_mut(&mut parsed);
        let mut derive_from = DeriveFrom::default();
        derive_from.visit_file_mut(&mut parsed);
        derive_from.complete(&mut parsed);

        let generated = prettyplease::unparse(&parsed);
        let generated_path = outdir.join(raw_src.file_name().expect("file_name"));

        fs::write(generated_path, generated)?;
    }

    Ok(())
}
