use proc_macro::TokenStream;
use quote::{ToTokens, quote};
use syn::{DeriveInput, Meta, MetaList, parse_macro_input};

#[proc_macro_derive(State)]
pub fn state(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let expanded = quote! {
        impl #impl_generics State for #name #ty_generics #where_clause {}
    };

    TokenStream::from(expanded)
}

#[proc_macro_derive(Stateful, attributes(state))]
pub fn stateful(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let state_attrs: Vec<_> = input
        .attrs
        .iter()
        .filter(|attr| attr.style == syn::AttrStyle::Outer)
        .map(|attr| attr.path())
        .filter_map(|path| path.get_ident())
        .filter(|ident| ident.to_token_stream().to_string() == "state")
        .collect();
    if state_attrs.len() != 1 {
        panic!("#[derive(Stateful)] requires specifying the state with #[state(<State>)] attribute")
    }

    let state_toks: Vec<_> = match input.attrs[0].meta.clone() {
        Meta::List(MetaList { tokens, .. }) => tokens.into_iter().collect(),
        _ => panic!("#[state(<State>)] is the proper way to describe the state"),
    };

    let state = if state_toks.len() != 1 {
        panic!("Only one state allowed in #[state(<State>)] attribtue")
    } else {
        &state_toks[0]
    };

    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let expanded = quote! {
        impl #impl_generics Stateful for #name #ty_generics #where_clause {
            type State = #state;
        }
    };

    TokenStream::from(expanded)
}

#[proc_macro_derive(SelfTransitionable)]
pub fn self_transitionable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    let name = input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let expanded = quote! {
        impl #impl_generics SelfTransitionable for #name #ty_generics #where_clause {
        }
    };

    TokenStream::from(expanded)
}
