//! Derive macros for the `nitinol` event-sourcing framework.
//!
//! `#[derive(Event)]` is pure sugar for the hand-written `impl Event` pattern:
//! it generates the type-level `EVENT_TYPE` const, the associated `Codec`, and,
//! for enums, a per-arm `variant()` match. It provides no macro-specific
//! behaviour — hand-written implementations remain equally valid.
//!
//! Alongside the impl it places one
//! `nitinol::persistence::EventRegistryEntry` in
//! `nitinol::persistence::EVENT_REGISTRY`, carrying the type-level identity and
//! every arm — every canonical path a stored row of this type can carry. That
//! is what lets a store restore the type with nothing registered by hand, and a
//! hand-written `impl Event` places the same entry itself.
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
use quote::{format_ident, quote};
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

    let type_level = quote! {
        ::nitinol::persistence::EventType::new(
            ::nitinol::persistence::Family::new(#family),
            ::nitinol::persistence::TypeName::new(stringify!(#ident)),
        )
    };

    // One spelling of an arm's identity, used both by `variant()` and by the
    // registry entry: they are the same path, and a row carries it.
    let arm_level = |arm_ident: &syn::Ident| {
        quote! {
            ::nitinol::persistence::EventType::with_variant(
                ::nitinol::persistence::Family::new(#family),
                ::nitinol::persistence::TypeName::new(stringify!(#ident)),
                ::nitinol::persistence::Variant::new(stringify!(#arm_ident)),
            )
        }
    };

    // `None` for a struct, which inherits the trait's default `variant()`.
    let arms: Option<Vec<&syn::Variant>> = match &input.data {
        Data::Struct(_) => None,
        Data::Enum(data) => Some(data.variants.iter().collect()),
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(
                ident,
                "#[derive(Event)] does not support unions",
            ));
        }
    };

    let variant_fn = match &arms {
        None => quote! {},
        Some(arms) => {
            let matched = arms.iter().map(|variant| {
                let arm_ident = &variant.ident;
                let binding = match &variant.fields {
                    Fields::Unit => quote!(),
                    Fields::Unnamed(_) => quote!((..)),
                    Fields::Named(_) => quote!({ .. }),
                };
                let arm_level = arm_level(arm_ident);
                quote! {
                    Self::#arm_ident #binding => #arm_level,
                }
            });
            quote! {
                fn variant(&self) -> ::nitinol::persistence::EventType {
                    match self {
                        #(#matched)*
                    }
                }
            }
        }
    };

    // Every canonical path this type is written down as, placed where the
    // linker gathers it: the type-level identity a hand-written `SystemEvent`
    // journals under, and each arm, which is what a row of an enum carries.
    let declared = arms
        .iter()
        .flatten()
        .map(|variant| arm_level(&variant.ident));
    let entry = format_ident!("_NITINOL_EVENT_REGISTRY_{}", ident);
    let registry_entry = quote! {
        #[::nitinol::__private::linkme::distributed_slice(::nitinol::persistence::EVENT_REGISTRY)]
        #[linkme(crate = ::nitinol::__private::linkme)]
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #entry: ::nitinol::persistence::EventRegistryEntry =
            ::nitinol::persistence::EventRegistryEntry::new(
                concat!(module_path!(), "::", stringify!(#ident)),
                &[#type_level #(, #declared)*],
            );
    };

    Ok(quote! {
        impl #impl_generics ::nitinol::contract::Event for #ident #ty_generics #where_clause {
            const EVENT_TYPE: ::nitinol::persistence::EventType = #type_level;

            type Codec = #codec;

            #variant_fn
        }

        #registry_entry
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
