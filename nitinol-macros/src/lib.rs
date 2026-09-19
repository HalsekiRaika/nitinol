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
//! It also exports one symbol named after that path, which is what makes a
//! second declaration of the same path a build failure rather than a
//! deployment that restores one declaration's rows as the other's. See the
//! `Event` derive for the two messages that collision is reported with.
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

use nitinol_persistence::__private::{
    is_verbatim_event_symbol_byte, EVENT_UNIQUENESS_SYMBOL_PREFIX,
};
use nitinol_persistence::{is_within_reserved_namespace, RESERVED_NAMESPACE};

/// Derives `nitinol::contract::Event` for a struct or enum.
///
/// Requires `#[event(family = "...", codec = ...)]`. A struct and an enum get
/// the same shape of identity: the attribute's family, and the type's own
/// identifier.
///
/// # One declaration per canonical path
///
/// A canonical path answers for one event type, so two declarations claiming
/// one family and one type name between them are refused before the program
/// runs. The derive exports a symbol named after the path — `_nitinol_event_`
/// and the path itself — so the second declaration is a symbol already defined.
///
/// Within one crate that is a `rustc` error at the second declaration:
///
/// ```text
/// error: symbol `_nitinol_event_shop.order.Opened` is already defined
///   --> src/billing.rs:24:21
///    |
/// 24 |     #[derive(Clone, Event)]
///    |                     ^^^^^
///    |
///    = note: this error originates in the derive macro `Event` (in Nightly builds, run with -Z macro-backtrace for more info)
/// ```
///
/// Across crates it is the linker's duplicate-symbol error, reported once the
/// two objects meet:
///
/// ```text
/// error: linking with `cc` failed: exit status: 1
///   |
///   = note: rust-lld: error: duplicate symbol: _nitinol_event_shop.order.Opened
///           >>> defined at libshop_orders.rlib(shop_orders.o)
///           >>> defined at libshop_billing.rlib(shop_billing.o)
/// ```
///
/// Either message names the path both declarations claimed; renaming the
/// family or the type of one of them is what resolves it. Which module a type
/// is defined in is no part of its identity, so moving one of them does not.
///
/// # Why the registry still refuses the same collision
///
/// Link-time detection is the first of two layers, not the only one. It has
/// holes — static libraries linked together need not report a duplicate at all
/// — and an entry placed by hand rather than derived carries no such symbol.
/// The registry the framework builds for the process therefore runs the same
/// check over the entries it collected, and a store opened over a binary whose
/// declarations collide refuses to open.
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

    // One symbol per canonical path, so a second declaration of the same path
    // is a definition rustc or the linker already refuses. `#[used]` is what
    // keeps it in the object file a declaration that claims nothing else of the
    // binary still has to be caught through.
    let claim = format_ident!("_NITINOL_EVENT_CLAIM_{}", ident);
    let symbol = uniqueness_symbol(&family.value(), &ident.to_string());
    let path_claim = quote! {
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        #[used]
        #[export_name = #symbol]
        static #claim: u8 = 0;
    };

    Ok(quote! {
        impl #impl_generics ::nitinol::contract::Event for #ident #ty_generics #where_clause {
            const EVENT_TYPE: ::nitinol::persistence::EventType = #event_type;

            type Codec = #codec;
        }

        #registry_entry

        #path_claim
    })
}

/// The symbol a declaration claims its canonical path with.
///
/// The prefix and the verbatim byte set belong to `nitinol-persistence`, which
/// is also where the encoding and its injectivity are written down; what is
/// here is the escape that rule leaves to the caller. A byte outside the set is
/// written as `..` and two hex digits, which no path produces on its own: a
/// family rejects consecutive dots and a type name rejects dots outright.
fn uniqueness_symbol(family: &str, type_name: &str) -> String {
    let mut symbol = String::from(EVENT_UNIQUENESS_SYMBOL_PREFIX);
    let path = if family.is_empty() {
        type_name.to_owned()
    } else {
        format!("{family}.{type_name}")
    };

    for byte in path.bytes() {
        if is_verbatim_event_symbol_byte(byte) {
            symbol.push(byte as char);
        } else {
            symbol.push_str("..");
            symbol.push_str(&format!("{byte:02X}"));
        }
    }

    symbol
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

#[cfg(test)]
mod uniqueness_symbol_tests {
    use super::{is_verbatim_event_symbol_byte, uniqueness_symbol};

    /// The whole point of the symbol: two declarations get one name only when
    /// they are written down as one path. A sanitizer folding `.` onto `_`
    /// would name these two the same thing, and one of them would never be
    /// built.
    #[test]
    fn two_families_a_dot_apart_are_named_apart() {
        assert_ne!(
            uniqueness_symbol("shop.registry.sub", "Leaf"),
            uniqueness_symbol("shop.registry_sub", "Leaf"),
        );
    }

    /// A family may hold bytes a symbol cannot. They are escaped rather than
    /// dropped or folded, so the separators they sit beside stay separators and
    /// the neighbours a naive sanitizer would collapse them onto stay apart.
    #[test]
    fn a_byte_outside_the_verbatim_set_is_escaped_and_stays_distinct() {
        let escaped = uniqueness_symbol("shop-1.order", "Opened");

        assert_eq!(escaped, "_nitinol_event_shop..2D1.order.Opened");
        assert_ne!(escaped, uniqueness_symbol("shop.1.order", "Opened"));
        assert_ne!(escaped, uniqueness_symbol("shop_1.order", "Opened"));
    }

    /// The symbol reaches a linker, so it holds only the bytes one accepts:
    /// ASCII alphanumerics, `_` and `.`, whatever the family was written with.
    #[test]
    fn every_byte_of_a_symbol_is_one_a_linker_accepts() {
        let symbol = uniqueness_symbol("shop-1.ordre\u{300}s", "Opened");

        assert!(
            symbol.bytes().all(is_verbatim_event_symbol_byte),
            "a family may be written with any byte a string literal holds, and none of them \
             reaches the symbol unescaped. It produced {symbol:?}"
        );
    }

    /// An empty family is a supported value, and the path it produces is the
    /// type name alone — not a leading separator the encoding would carry into
    /// the symbol.
    #[test]
    fn an_empty_family_is_named_by_the_type_name_alone() {
        assert_eq!(
            uniqueness_symbol("", "Incremented"),
            "_nitinol_event_Incremented",
        );
    }
}
