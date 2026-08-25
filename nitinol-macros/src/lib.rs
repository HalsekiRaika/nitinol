//! Derive macros for the `nitinol` event-sourcing framework.
//!
//! `#[derive(Event)]` is pure sugar for the hand-written `impl Event` pattern:
//! it generates the type-level `EVENT_TYPE` const and, for enums, a per-arm
//! `variant()` match. It provides no macro-specific behaviour — hand-written
//! implementations remain equally valid.
//!
//! The family segment cannot be recovered from a type's module path by a
//! proc-macro derive, so it is supplied explicitly via `#[event(family = "...")]`.
//!
//! Generated code refers to the framework exclusively through the umbrella
//! crate (`::nitinol::eventsource::Event`, `::nitinol::persistence::{...}`),
//! mirroring how `serde`'s derive targets the single `serde` crate.
//!
//! # Reserved namespace
//!
//! `nitinol` is reserved for the framework's own records in both the stream-key
//! and the event-type space (see `nitinol_persistence::reserved`).  Here the
//! family is a literal known at expansion time, so the derive refuses it
//! outright: a family inside the reserved namespace is a compile error, not a
//! runtime surprise.  A hand-written `impl Event` bypasses this derive and so
//! is bound by the same rule as documentation only.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Lit, LitStr};

use nitinol_persistence::{is_within_reserved_namespace, RESERVED_NAMESPACE};

/// Derives `nitinol::eventsource::Event` for a struct or enum.
///
/// Requires `#[event(family = "...")]`. Structs inherit the trait's default
/// `variant()`; enums additionally get a `variant()` that maps each arm to its
/// identifier.
#[proc_macro_derive(Event, attributes(event))]
pub fn derive_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let family = extract_family(&input)?;
    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let event_type_const = quote! {
        const EVENT_TYPE: ::nitinol::persistence::EventType =
            ::nitinol::persistence::EventType::new(
                ::nitinol::persistence::Family::new(#family),
                ::nitinol::persistence::TypeName::new(stringify!(#ident)),
            );
    };

    let variant_fn = match &input.data {
        Data::Struct(_) => quote! {},
        Data::Enum(data) => {
            let arms = data.variants.iter().map(|variant| {
                let arm_ident = &variant.ident;
                let binding = match &variant.fields {
                    Fields::Unit => quote!(),
                    Fields::Unnamed(_) => quote!((..)),
                    Fields::Named(_) => quote!({ .. }),
                };
                quote! {
                    Self::#arm_ident #binding => ::nitinol::persistence::EventType::with_variant(
                        ::nitinol::persistence::Family::new(#family),
                        ::nitinol::persistence::TypeName::new(stringify!(#ident)),
                        ::nitinol::persistence::Variant::new(stringify!(#arm_ident)),
                    ),
                }
            });
            quote! {
                fn variant(&self) -> ::nitinol::persistence::EventType {
                    match self {
                        #(#arms)*
                    }
                }
            }
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(Event)] does not support unions",
            ));
        }
    };

    Ok(quote! {
        impl #impl_generics ::nitinol::eventsource::Event for #ident #ty_generics #where_clause {
            #event_type_const
            #variant_fn
        }
    })
}

/// Extracts the mandatory `#[event(family = "...")]` string literal, refusing
/// one that names the framework's reserved namespace.
///
/// A missing attribute, a non-string value, or a reserved family is a hard
/// error (Fail Fast): the derive never falls back to a default family, and
/// never lets a user event take an identity `nitinol`'s own records answer to.
fn extract_family(input: &DeriveInput) -> syn::Result<LitStr> {
    let mut family: Option<LitStr> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("event") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if !meta.path.is_ident("family") {
                return Err(meta.error("unknown `event` attribute key; expected `family`"));
            }
            let value = meta.value()?;
            match value.parse::<Lit>()? {
                Lit::Str(lit) => {
                    family = Some(lit);
                    Ok(())
                }
                other => Err(syn::Error::new_spanned(
                    other,
                    "expected a string literal for `#[event(family = \"...\")]`",
                )),
            }
        })?;
    }

    let family = family.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "#[derive(Event)] requires a `#[event(family = \"...\")]` attribute",
        )
    })?;

    if is_within_reserved_namespace(&family.value()) {
        return Err(syn::Error::new_spanned(
            &family,
            format!(
                "`family` is within the `{RESERVED_NAMESPACE}` reserved namespace, \
                 which is reserved for framework events"
            ),
        ));
    }

    Ok(family)
}
