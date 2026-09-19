//! Derive macros for the `nitinol` event-sourcing framework.
//!
//! `#[derive(Event)]` is pure sugar for the hand-written `impl Event` pattern:
//! it generates the `EVENT_TYPE` const and the associated `Codec`. It provides
//! no macro-specific behaviour — hand-written implementations remain equally
//! valid.
//!
//! An enum is named after the enum, not after whichever arm a value happens to
//! hold: a row carries the declaration's path, and which arm it holds is
//! carried by the payload the named codec wrote.
//!
//! Alongside the impl it places one
//! `nitinol::persistence::EventRegistryEntry` in
//! `nitinol::persistence::EVENT_REGISTRY`, carrying the one canonical path a
//! stored row of this type can carry. That is what lets a store restore the
//! type with nothing registered by hand, and a hand-written `impl Event` places
//! the same entry itself.
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
//! family is a literal known at expansion time, so the derive reads the same
//! central rule (`nitinol_persistence::is_within_reserved_namespace`) and
//! refuses it before expansion: a family inside the reserved namespace is a
//! compile error reported at the derive site, not a runtime surprise.
//!
//! A hand-written `impl Event` bypasses this derive, but not the rule: it still
//! has to build its `EventType` through `Family::new` and its registry entry
//! through `EventRegistryEntry::new`, and both panic while the constant holding
//! them is evaluated if the family names the reserved namespace — a compile
//! error of a different shape, not a documentation-only convention. The one way
//! past either enforcement point is `nitinol_persistence::__private`, reserved
//! for the framework's own records and not a user surface.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Lit, LitStr, Type};

use nitinol_persistence::{is_within_reserved_namespace, RESERVED_NAMESPACE};

/// Derives `nitinol::contract::Event` for a struct or enum.
///
/// Requires `#[event(family = "...", codec = ...)]`. A struct and an enum get
/// the same shape of identity: the attribute's family, and the type's own
/// identifier.
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

    // A union has no `Codec` a caller could name and no identity to derive from
    // its shape, so it is refused rather than expanded over.
    if let Data::Union(_) = &input.data {
        return Err(syn::Error::new_spanned(
            ident,
            "#[derive(Event)] does not support unions",
        ));
    }

    let event_type = quote! {
        ::nitinol::persistence::EventType::new(
            ::nitinol::persistence::Family::new(#family),
            ::nitinol::persistence::TypeName::new(stringify!(#ident)),
        )
    };

    // The canonical path this type is written down as, placed where the linker
    // gathers it. One declaration, one path: a row of an enum carries this too,
    // and which arm it holds is the payload's to say.
    let entry = format_ident!("_NITINOL_EVENT_REGISTRY_{}", ident);
    let registry_entry = quote! {
        #[::nitinol::__private::linkme::distributed_slice(::nitinol::persistence::EVENT_REGISTRY)]
        #[linkme(crate = ::nitinol::__private::linkme)]
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #entry: ::nitinol::persistence::EventRegistryEntry =
            ::nitinol::persistence::EventRegistryEntry::new(
                concat!(module_path!(), "::", stringify!(#ident)),
                &[#event_type],
            );
    };

    Ok(quote! {
        impl #impl_generics ::nitinol::contract::Event for #ident #ty_generics #where_clause {
            const EVENT_TYPE: ::nitinol::persistence::EventType = #event_type;

            type Codec = #codec;
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
