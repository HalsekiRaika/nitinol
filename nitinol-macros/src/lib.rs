//! Derive macros for the `nitinol` event-sourcing framework.
//!
//! `#[derive(Event)]` is pure sugar for the hand-written `impl Event` pattern:
//! it generates the type-level `EVENT_TYPE` const, the associated `Codec`, and,
//! for enums, a per-arm `variant()` match. It provides no macro-specific
//! behaviour — hand-written implementations remain equally valid.
//!
//! The family segment cannot be recovered from a type's module path by a
//! proc-macro derive, so it is supplied explicitly via `#[event(family = "...")]`.
//! The codec is supplied the same way, by `#[event(codec = ...)]`, and copied
//! into `type Codec` verbatim: which format writes an event down is a decision
//! the domain makes, and there is nothing in the shape of a type for a derive to
//! infer it from.
//!
//! Generated code refers to the framework exclusively through the umbrella
//! crate (`::nitinol::contract::Event`, `::nitinol::persistence::{...}`),
//! mirroring how `serde`'s derive targets the single `serde` crate.  Both
//! paths are reachable without an async runtime, so applying this derive never
//! obliges the user to enable `eventsource` — and with it Tokio.
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
use syn::{parse_macro_input, Data, DeriveInput, Fields, Lit, LitStr, Type};

use nitinol_persistence::{is_within_reserved_namespace, RESERVED_NAMESPACE};

/// Derives `nitinol::contract::Event` for a struct or enum.
///
/// Requires `#[event(family = "...", codec = ...)]`. Structs inherit the
/// trait's default `variant()`; enums additionally get a `variant()` that maps
/// each arm to its identifier.
#[proc_macro_derive(Event, attributes(event))]
pub fn derive_event(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let EventAttrs { family, codec } = extract_attrs(&input)?;
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
        impl #impl_generics ::nitinol::contract::Event for #ident #ty_generics #where_clause {
            #event_type_const

            type Codec = #codec;

            #variant_fn
        }
    })
}

/// What `#[event(...)]` must state for the derive to write an `impl Event`.
struct EventAttrs {
    family: LitStr,
    codec: Type,
}

/// Extracts the mandatory `family` and `codec` keys of `#[event(...)]`, refusing
/// a family that names the framework's reserved namespace.
///
/// A missing key, a non-string family, or a reserved family is a hard error
/// (Fail Fast): the derive never falls back to a default family, never lets a
/// user event take an identity `nitinol`'s own records answer to, and never
/// invents a codec — stable Rust has no associated-type defaults, so an omitted
/// `codec` would otherwise be reported as a missing associated type rather than
/// as the attribute the author left out.
fn extract_attrs(input: &DeriveInput) -> syn::Result<EventAttrs> {
    let mut family: Option<LitStr> = None;
    let mut codec: Option<Type> = None;

    for attr in &input.attrs {
        if !attr.path().is_ident("event") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("family") {
                let value = meta.value()?;
                return match value.parse::<Lit>()? {
                    Lit::Str(lit) => {
                        family = Some(lit);
                        Ok(())
                    }
                    other => Err(syn::Error::new_spanned(
                        other,
                        "expected a string literal for `#[event(family = \"...\")]`",
                    )),
                };
            }
            if meta.path.is_ident("codec") {
                codec = Some(meta.value()?.parse::<Type>()?);
                return Ok(());
            }
            Err(meta.error("unknown `event` attribute key; expected `family` or `codec`"))
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

    let codec = codec.ok_or_else(|| {
        syn::Error::new_spanned(
            &input.ident,
            "#[derive(Event)] requires a `#[event(codec = ...)]` attribute",
        )
    })?;

    Ok(EventAttrs { family, codec })
}
