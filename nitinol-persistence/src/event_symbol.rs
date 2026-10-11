//! The symbol a declaration's canonical path is named by, and the macro that
//! writes one for a framework-owned record.
//!
//! # Why a symbol at all
//!
//! One canonical path answers for one event type. A declaration therefore
//! exports one symbol derived from its path, and a second declaration claiming
//! that path exports the same symbol — which is a definition the toolchain
//! already refuses: rustc reports `symbol ... is already defined` when both are
//! in one crate, and the linker reports a duplicate symbol when they are in
//! two. Either way the collision is a failed build rather than a deployment
//! that restores one declaration's rows as the other's.
//!
//! [`EventRegistry`](crate::EventRegistry) is the second layer, not a spare
//! one: link-time detection has holes (static libraries linked together need
//! not report a duplicate), and an entry placed by hand carries no such symbol
//! at all.
//!
//! # The encoding
//!
//! The symbol is [`EVENT_UNIQUENESS_SYMBOL_PREFIX`] followed by the canonical
//! path, which keeps it short enough to read in a linker's message and in a
//! debugger. A path is already close to symbol-shaped: every byte that is
//! ASCII alphanumeric, `_`, or `.` is carried verbatim, and any other byte is
//! written as `..` and two uppercase hex digits.
//!
//! The encoding is injective, so no two paths are named the same thing:
//!
//! - a path carries no `..` of its own — a family rejects consecutive dots and
//!   a type name rejects dots outright — so every `..` in a symbol begins an
//!   escape, and a hex digit is never `.`, so where an escape begins is never
//!   in question;
//! - `shop.registry.sub.Leaf` and `shop.registry_sub.Leaf` are two paths and
//!   two symbols, which a sanitizer that folded `.` onto `_` would not give.

/// What marks a symbol as nitinol's claim on one canonical path.
///
/// Short on purpose: an over-long symbol is what a linker error and a debugger
/// both have to show, and the path that follows is the part a reader needs.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface. `#[derive(Event)]` reads it to name the symbol it writes for a
/// user declaration.
pub const EVENT_UNIQUENESS_SYMBOL_PREFIX: &str = "_nitinol_event_";

/// Is `byte` one a uniqueness symbol carries as itself rather than as an
/// escape?
///
/// The one place the verbatim set is stated. `#[derive(Event)]` reads it to
/// encode a user's canonical path, which may hold any byte a family literal
/// holds.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface.
pub const fn is_verbatim_event_symbol_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'.'
}

/// Is every byte of `s` one a uniqueness symbol carries verbatim?
///
/// [`reserved_event!`](crate::reserved_event) builds its symbol by
/// concatenating literals, which is all `concat!` can do — so it asserts this
/// rather than escaping, and a framework record named outside the verbatim set
/// is a compile error instead of a symbol nothing else would have produced.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface.
pub const fn is_verbatim_event_symbol(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut nth = 0;
    while nth < bytes.len() {
        if !is_verbatim_event_symbol_byte(bytes[nth]) {
            return false;
        }
        nth += 1;
    }
    true
}

/// Declare an event type the framework owns: its identity, its registry entry,
/// its claim on the canonical path, and — for a record with more than one wire
/// shape — the arm names its payload's `oneof` is reported under.
///
/// The counterpart of `#[derive(Event)]` for records that carry the names the
/// framework reserved. It expands to the [`EventType`](crate::EventType)
/// constant it is named after, the [`EventRegistryEntry`](crate::EventRegistryEntry)
/// that puts the record in [`EVENT_REGISTRY`](crate::EVENT_REGISTRY), and the
/// symbol that makes a second declaration of the same path a build failure.
///
/// ```
/// nitinol_persistence::reserved_event! {
///     /// The one identity every outbox marker is written down as.
///     const OUTBOX_MARKER {
///         family = "nitinol.example",
///         type_name = "outbox",
///         declared_by = OutboxEvent,
///     }
/// }
///
/// fn main() {
///     assert_eq!(OUTBOX_MARKER.to_string(), "nitinol.example.outbox");
/// }
/// ```
///
/// `declared_by` names the type whose codec writes and reads the record; it is
/// what a collision is reported with, and nothing resolves against it.
///
/// # Arms
///
/// A record whose payload is a prost `oneof` lists its transitions after the
/// identity block as `pattern => visibility name = "literal"`. The macro turns
/// each arm into an `&'static str` constant and folds all of them into one
/// inherent `arm()` on `arm_type`:
///
/// ```
/// nitinol_persistence::reserved_event! {
///     const OUTBOX_MARKER {
///         family = "nitinol.example",
///         type_name = "outbox",
///         declared_by = OutboxEvent,
///     }
///
///     arm_type = OutboxEvent;
///     OutboxEvent::TellRequested => pub(crate) TELL_REQUESTED_ARM = "tell_requested",
///     OutboxEvent::Ended => ENDED_ARM = "ended",
/// }
///
/// enum OutboxEvent {
///     TellRequested,
///     Ended,
/// }
///
/// fn main() {
///     assert_eq!(OutboxEvent::Ended.arm(), "ended");
///     assert_eq!(TELL_REQUESTED_ARM, "tell_requested");
/// }
/// ```
///
/// `arm_type` is independent of `declared_by`: a record whose arm is carried by
/// a nested type names that nested type here, and a pattern may look into a
/// field to reach it (`Self { kind: Kind::Processed, .. }`, say) rather than
/// matching the arm type itself. Omitting the visibility before an
/// arm's name keeps that constant private to the declaring module; only the
/// arms another module names directly need one.
///
/// # One call per module
///
/// The registry entry and the uniqueness symbol are written to the fixed
/// identifiers `ENTRY` and `CLAIMS_THE_PATH` inside an anonymous `const`
/// (`macro_rules!` cannot invent a fresh identifier per call), so a second
/// invocation in the same module redeclares them. Reserve one module per call.
///
/// Reachable only through `nitinol_persistence::__private`; it is not a user
/// surface. A user declaration reaches the same three things through
/// `#[derive(Event)]`, which is refused the reserved namespace.
#[doc(hidden)]
#[macro_export]
macro_rules! reserved_event {
    (
        $(#[$meta:meta])*
        $vis:vis const $ident:ident {
            family = $family:literal,
            type_name = $type_name:literal,
            declared_by = $declared_by:ident $(,)?
        }
        $(
            arm_type = $arm_ty:ty;
            $(
                $arm_pat:pat => $arm_vis:vis $arm_name:ident = $arm_lit:literal
            ),+ $(,)?
        )?
    ) => {
        $(#[$meta])*
        $vis const $ident: $crate::EventType = $crate::EventType::new(
            $crate::__private::reserved_family($family),
            $crate::TypeName::new($type_name),
        );

        const _: () = {
            #[$crate::__private::linkme::distributed_slice($crate::EVENT_REGISTRY)]
            #[linkme(crate = $crate::__private::linkme)]
            static ENTRY: $crate::EventRegistryEntry =
                $crate::__private::reserved_event_registry_entry(
                    concat!(module_path!(), "::", stringify!($declared_by)),
                    &[$ident],
                );

            // `concat!` takes literals, so the prefix is spelled here as well
            // as in `EVENT_UNIQUENESS_SYMBOL_PREFIX`; the assertions below are
            // what keep a reserved name inside the shape this spelling encodes.
            #[used]
            #[export_name = concat!("_nitinol_event_", $family, ".", $type_name)]
            static CLAIMS_THE_PATH: u8 = 0;

            assert!(
                $crate::is_within_reserved_namespace($family),
                "this declares a record the framework owns, and the names it owns are the ones \
                 within the `nitinol` namespace; a record outside it belongs to `#[derive(Event)]`"
            );
            assert!(
                $crate::__private::is_verbatim_event_symbol($family)
                    && $crate::__private::is_verbatim_event_symbol($type_name),
                "a reserved event's family and type name are carried into its uniqueness symbol \
                 verbatim; name the record with ASCII alphanumerics, `_`, and `.` only"
            );
        };

        $(
            $( $arm_vis const $arm_name: &'static str = $arm_lit; )+

            impl $arm_ty {
                /// The wire shape this value is, as the name its payload's
                /// `oneof` arm is reported under.
                pub(crate) const fn arm(&self) -> &'static str {
                    match self {
                        $( $arm_pat => $arm_name, )+
                    }
                }
            }
        )?
    };
}
