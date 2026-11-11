//! Expansion of `#[derive(SagaSubscribe)]`.
//!
//! The derive reads an alphabet declaration — an enum whose newtype variants
//! name the inputs a saga reacts to — and writes three things from it:
//!
//! 1. the `SagaSubscribe` impl, and — for a `#[subscribe(deadline)]` variant —
//!    the `DeadlineVariant` impl that lets the framework read a firing back
//!    as that variant;
//! 2. a binding struct with one slot per declared external source;
//! 3. the slot transitions, so binding a source moves exactly its own slot and
//!    `spawn` exists only on the shape where every slot is filled.
//!
//! The binding struct has to be generated rather than assembled from tuples:
//! the slot-transition trait is foreign to the declaring crate, and the orphan
//! rule forbids implementing it for a foreign tuple type there.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Ident, Type, Variant};

/// One variant of the alphabet, classified.
struct Source {
    /// The variant, for the constructor the decode branch wraps with and for
    /// the span an error about it is reported at.
    variant: Ident,
    /// The single input type the variant names.
    input: Type,
}

/// The alphabet, as the expansion needs it.
struct Alphabet {
    /// The external sources, in declaration order.  One subscription, one
    /// cursor and one binding slot each.
    external: Vec<Source>,
    /// The saga's own time, when the alphabet declares it.  It is addressed
    /// directly rather than observed, so it is allotted no slot.
    deadline: Option<Source>,
}

pub(crate) fn expand(input: DeriveInput) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let vis = &input.vis;
    let alphabet = read_alphabet(&input)?;

    let bindings_ident = format_ident!("{}Bindings", ident);
    let slot_params: Vec<Ident> = (0..alphabet.external.len())
        .map(|index| format_ident!("__Slot{}", index))
        .collect();
    let slot_fields: Vec<Ident> = (0..alphabet.external.len())
        .map(|index| format_ident!("__slot_{}", index))
        .collect();

    let bindings = declare_bindings(vis, &bindings_ident, &slot_params, &slot_fields);
    let transitions = declare_transitions(
        &bindings_ident,
        &slot_params,
        &slot_fields,
        &alphabet.external,
    );
    let complete = declare_complete(ident, &bindings_ident, &slot_fields, &alphabet.external);
    let subscribe = declare_subscribe(ident, &bindings_ident, alphabet.deadline.as_ref());

    Ok(quote! {
        #bindings
        #transitions
        #complete
        #subscribe
    })
}

/// Classify the declaration, refusing every shape the generated code could not
/// be written for.
///
/// Each refusal is reported where the offending declaration is written, because
/// that is the only place it can be fixed.
fn read_alphabet(input: &DeriveInput) -> syn::Result<Alphabet> {
    let Data::Enum(data) = &input.data else {
        // An alphabet is a choice between inputs.  A struct names one shape,
        // not a set of them, and a union names no shape a codec could read.
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(SagaSubscribe)] supports enums only",
        ));
    };

    if !input.generics.params.is_empty() {
        // Each declared source is resolved to one event type at declaration
        // time — that is what the decode branch and the binding slot are built
        // from.  A generic alphabet leaves the set of sources open until
        // instantiation, so neither can be generated.
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(SagaSubscribe)] does not support generic enums",
        ));
    }

    let mut external: Vec<Source> = Vec::new();
    let mut deadline: Option<Source> = None;

    for variant in &data.variants {
        let source = read_source(variant)?;
        if !is_deadline(variant)? {
            external.push(source);
            continue;
        }
        // A saga has one clock.  Two deadline variants would give the schedule
        // fact two readings of the same bytes, with nothing on the wire to say
        // which arm a firing belongs to.
        if deadline.is_some() {
            return Err(syn::Error::new_spanned(
                &variant.ident,
                "#[derive(SagaSubscribe)] allows at most one `#[subscribe(deadline)]` variant",
            ));
        }
        deadline = Some(source);
    }

    if external.is_empty() {
        // An alphabet with nothing to observe declares no source, so its
        // binding typestate would be complete before anything was bound and
        // "spawning with no subscription is a compile error" would stop
        // holding.  The rule generalises to several sources rather than being
        // retired by them.
        return Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(SagaSubscribe)] requires at least one external source variant",
        ));
    }

    reject_duplicate_sources(&external)?;

    Ok(Alphabet { external, deadline })
}

/// The one input type a variant names.
fn read_source(variant: &Variant) -> syn::Result<Source> {
    let Fields::Unnamed(fields) = &variant.fields else {
        return Err(unnamed_single_field_error(variant));
    };
    if fields.unnamed.len() != 1 {
        return Err(unnamed_single_field_error(variant));
    }
    Ok(Source {
        variant: variant.ident.clone(),
        input: fields
            .unnamed
            .first()
            .expect("the length was just checked to be one")
            .ty
            .clone(),
    })
}

/// Every variant names exactly one input type: that type is what the decode
/// branch reads and what the binding slot is keyed by.  A unit variant names
/// none, and a variant naming several has no single type to subscribe to.
fn unnamed_single_field_error(variant: &Variant) -> syn::Error {
    syn::Error::new_spanned(
        &variant.ident,
        "#[derive(SagaSubscribe)] requires every variant to name exactly one input type",
    )
}

/// Two declared sources may not name the same event type.
///
/// A declared source owns a subscription and a cursor of its own, and two
/// variants over one type cannot have that: every record of the type matches
/// both, so either it is delivered twice — two decisions from one input — or
/// one of them is silently never delivered.  It is also where the binding
/// typestate stops being expressible, since a slot is keyed by its type.
///
/// Compared on the written type rather than on a resolved one: a derive cannot
/// resolve a path to a type, so two spellings of one type are not caught here —
/// the overlapping slot-transition impls the expansion writes are what refuse
/// those, with a coherence error instead of this message.
fn reject_duplicate_sources(external: &[Source]) -> syn::Result<()> {
    let mut seen: Vec<String> = Vec::with_capacity(external.len());
    for source in external {
        let input = &source.input;
        let written = quote!(#input).to_string();
        if seen.contains(&written) {
            return Err(syn::Error::new_spanned(
                &source.variant,
                "#[derive(SagaSubscribe)] declares the same event type more than once",
            ));
        }
        seen.push(written);
    }
    Ok(())
}

/// Does `#[subscribe(deadline)]` mark this variant as the saga's own time?
fn is_deadline(variant: &Variant) -> syn::Result<bool> {
    let mut deadline = false;
    for attr in &variant.attrs {
        if !attr.path().is_ident("subscribe") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("deadline") {
                deadline = true;
                return Ok(());
            }
            Err(meta.error("unknown `subscribe` attribute key; expected `deadline`"))
        })?;
    }
    Ok(deadline)
}

/// The binding typestate: one slot per declared external source, every slot
/// unbound to begin with.
fn declare_bindings(
    vis: &syn::Visibility,
    bindings: &Ident,
    slot_params: &[Ident],
    slot_fields: &[Ident],
) -> TokenStream {
    quote! {
        /// The binding typestate of the alphabet above: one slot per declared
        /// external source, written by `#[derive(SagaSubscribe)]`.
        ///
        /// Each `observing::<E>` call on the saga spawn builder moves this
        /// type's slot for `E` from unbound to bound, and `spawn` exists only on
        /// the shape where every slot is bound.
        #[doc(hidden)]
        #vis struct #bindings<
            #( #slot_params = ::nitinol::saga::SubscriptionUnset, )*
        > {
            #( #slot_fields: #slot_params, )*
        }

        impl ::core::default::Default for #bindings {
            fn default() -> Self {
                #bindings {
                    #( #slot_fields: ::nitinol::saga::SubscriptionUnset, )*
                }
            }
        }
    }
}

/// One slot transition per declared external source.
///
/// Each impl is written for the shape whose own slot is still unbound and whose
/// every other slot is anything, so binding a source twice has no impl to
/// resolve to and binding an undeclared source has none either.
fn declare_transitions(
    bindings: &Ident,
    slot_params: &[Ident],
    slot_fields: &[Ident],
    external: &[Source],
) -> TokenStream {
    let impls = external.iter().enumerate().map(|(bound, source)| {
        let input = &source.input;
        // The slot being bound is pinned to its state; every other slot stays a
        // parameter, so the transitions compose in any order.
        let others: Vec<&Ident> = slot_params
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != bound)
            .map(|(_, param)| param)
            .collect();
        let unbound_args = slot_state(
            slot_params,
            bound,
            quote!(::nitinol::saga::SubscriptionUnset),
        );
        let bound_args = slot_state(slot_params, bound, quote!(::nitinol::saga::SubscriptionSet));
        let moved = slot_fields.iter().enumerate().map(|(index, field)| {
            if index == bound {
                quote!( #field: ::nitinol::saga::SubscriptionSet(binding) )
            } else {
                quote!( #field: self.#field )
            }
        });
        quote! {
            impl< #( #others, )* > ::nitinol::saga::BindSource<#input>
                for #bindings< #( #unbound_args, )* >
            {
                type Bound = #bindings< #( #bound_args, )* >;

                fn bind(self, binding: ::nitinol::saga::SourceBinding) -> Self::Bound {
                    #bindings { #( #moved, )* }
                }
            }
        }
    });
    quote! { #( #impls )* }
}

/// The slot arguments with the slot at `bound` pinned to `state` and every other
/// left as its parameter.
fn slot_state(slot_params: &[Ident], bound: usize, state: TokenStream) -> Vec<TokenStream> {
    slot_params
        .iter()
        .enumerate()
        .map(|(index, param)| {
            if index == bound {
                state.clone()
            } else {
                quote!(#param)
            }
        })
        .collect()
}

/// The one shape with every declared external source bound, and the sources it
/// resolves to.
fn declare_complete(
    ident: &Ident,
    bindings: &Ident,
    slot_fields: &[Ident],
    external: &[Source],
) -> TokenStream {
    let all_bound = external
        .iter()
        .map(|_| quote!(::nitinol::saga::SubscriptionSet));
    let sources = external.iter().zip(slot_fields).map(|(source, field)| {
        let input = &source.input;
        let variant = &source.variant;
        quote! {
            ::nitinol::saga::SubscribedSource::new::<#input>(self.#field, #ident::#variant)
        }
    });
    quote! {
        impl ::nitinol::saga::AllSourcesSubscribed<#ident>
            for #bindings< #( #all_bound, )* >
        {
            fn into_sources(
                self,
            ) -> ::std::vec::Vec<::nitinol::saga::SubscribedSource<#ident>> {
                // Declaration order, so a reader of the spawn boundary sees the
                // sources in the order the alphabet lists them.
                ::std::vec![ #( #sources, )* ]
            }
        }
    }
}

/// The alphabet impl: which binding typestate it carries, and — for a
/// `#[subscribe(deadline)]` variant — the [`DeadlineVariant`] impl that lets
/// the framework read a firing back as that variant.
///
/// The read-back itself is not written here: `HasDeadline<M>`'s own
/// `DeclaresDeadline` impl builds it from this `DeadlineVariant` impl, so
/// there is one place a fired deadline is decoded regardless of whether the
/// alphabet was derived or hand-written.
fn declare_subscribe(ident: &Ident, bindings: &Ident, deadline: Option<&Source>) -> TokenStream {
    // `Deadline` is the compile-time half of the same declaration: fixed to
    // `HasDeadline<M>` for the one message type the deadline variant names, so
    // `SagaComponents::deadline::<M>()` exists only where this associated type
    // equals `HasDeadline<M>` — the same type the spawn boundary reads
    // `HAS_DEADLINE` from, so the component and the resolved timer core can
    // never disagree about whether this saga has its own time.
    let deadline_assoc_type = match deadline {
        Some(source) => {
            let message = &source.input;
            quote!(::nitinol::saga::HasDeadline<#message>)
        }
        None => quote!(::nitinol::saga::NoDeadline),
    };

    let deadline_variant_impl = deadline.map(|source| {
        let message = &source.input;
        let variant = &source.variant;
        quote! {
            impl ::nitinol::saga::DeadlineVariant<#message> for #ident {
                fn wrap_deadline(message: #message) -> Self {
                    #ident::#variant(message)
                }
            }
        }
    });

    quote! {
        impl ::nitinol::saga::SagaSubscribe for #ident {
            type Bindings = #bindings;
            type Deadline = #deadline_assoc_type;
        }

        #deadline_variant_impl
    }
}
