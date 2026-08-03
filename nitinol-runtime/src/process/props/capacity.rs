use std::num::NonZeroUsize;

use crate::error::ConfigError;

/// Default mailbox capacity used when a `MailboxCapacity::Inherit` value is
/// spawned outside any `ProcessSystem` (e.g. detached temp-ask actors).
///
/// Kept at the pre-spec hardcoded value (32) so that detached spawn paths do
/// not pick up surprise back-pressure from a process-system-level default.
pub(crate) const DETACHED_MAILBOX_CAPACITY: usize = 32;

/// Default pipe capacity for detached spawns. Same rationale as
/// [`DETACHED_MAILBOX_CAPACITY`].
pub(crate) const DETACHED_PIPE_CAPACITY: usize = 32;

/// Default stash capacity for detached spawns.
pub(crate) const DETACHED_STASH_CAPACITY: usize = 32;

macro_rules! capacity_enum {
    ($(#[$attr:meta])* $name:ident) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy)]
        pub enum $name {
            /// Read the matching default from the spawning `ProcessSystem`.
            Inherit,
            /// Use the supplied positive capacity at the channel boundary.
            Bounded(NonZeroUsize),
        }

        impl Default for $name {
            fn default() -> Self {
                Self::Inherit
            }
        }

        impl $name {
            /// Construct a `Bounded(NonZeroUsize)` from a `usize`, rejecting
            /// zero with `ConfigError::ZeroCapacity` so no zero-sized buffer
            /// can leak past the public boundary.
            pub fn bounded(n: usize) -> Result<Self, ConfigError> {
                NonZeroUsize::new(n)
                    .map(Self::Bounded)
                    .ok_or(ConfigError::ZeroCapacity)
            }
        }
    };
}

capacity_enum!(
    /// Capacity declaration for a process's user mailbox.
    ///
    /// `Inherit` defers to the spawning `ProcessSystem`'s default; `Bounded(n)`
    /// fixes the bounded channel size at `n`.
    MailboxCapacity
);

capacity_enum!(
    /// Capacity declaration for a process's stash buffer.
    ///
    /// The stash machinery itself is implemented in a follow-up issue; this
    /// enum reserves the API surface so call sites can compile today.
    StashCapacity
);

capacity_enum!(
    /// Capacity declaration for the `PipeDriver` bounded channel that backs
    /// [`crate::process::ProcessContext::pipe_to_self`].
    PipeCapacity
);

/// Resolve a `MailboxCapacity::Inherit` value against a system default,
/// returning the concrete bounded channel size.
pub(crate) fn resolve_mailbox(cap: MailboxCapacity, default: MailboxCapacity) -> NonZeroUsize {
    match cap {
        MailboxCapacity::Bounded(n) => n,
        MailboxCapacity::Inherit => decay_mailbox(default),
    }
}

/// Resolve a `PipeCapacity::Inherit` value against a system default,
/// returning the concrete bounded channel size.
pub(crate) fn resolve_pipe(cap: PipeCapacity, default: PipeCapacity) -> NonZeroUsize {
    match cap {
        PipeCapacity::Bounded(n) => n,
        PipeCapacity::Inherit => decay_pipe(default),
    }
}

/// Resolve a `StashCapacity::Inherit` value against a system default,
/// returning the concrete bounded channel size.
pub(crate) fn resolve_stash(cap: StashCapacity, default: StashCapacity) -> NonZeroUsize {
    match cap {
        StashCapacity::Bounded(n) => n,
        StashCapacity::Inherit => decay_stash(default),
    }
}

fn decay_mailbox(default: MailboxCapacity) -> NonZeroUsize {
    match default {
        MailboxCapacity::Bounded(n) => n,
        MailboxCapacity::Inherit => NonZeroUsize::new(DETACHED_MAILBOX_CAPACITY)
            .expect("DETACHED_MAILBOX_CAPACITY must be > 0"),
    }
}

fn decay_pipe(default: PipeCapacity) -> NonZeroUsize {
    match default {
        PipeCapacity::Bounded(n) => n,
        PipeCapacity::Inherit => {
            NonZeroUsize::new(DETACHED_PIPE_CAPACITY).expect("DETACHED_PIPE_CAPACITY must be > 0")
        }
    }
}

fn decay_stash(default: StashCapacity) -> NonZeroUsize {
    match default {
        StashCapacity::Bounded(n) => n,
        StashCapacity::Inherit => {
            NonZeroUsize::new(DETACHED_STASH_CAPACITY).expect("DETACHED_STASH_CAPACITY must be > 0")
        }
    }
}
