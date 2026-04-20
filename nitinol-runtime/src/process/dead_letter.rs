mod process;
mod throttle;

pub(crate) use self::process::{DeadLetterEnvelope, DeadLetterProcess, DeadLetterProxy};

use std::marker::PhantomData;

use crate::ident::Pid;
use crate::process::message::Boxed;

/// Implementing this trait suppresses dead-letter log output for the type.
/// Stream notification is NOT suppressed — messages still reach the stream.
pub trait SuppressDeadLetterLog {}

// Autoref specialization: stable-Rust way to detect the marker at call sites.

struct SuppressProbe<T>(PhantomData<T>);

// High priority: inherent method, only compiled when T: SuppressDeadLetterLog
impl<T: SuppressDeadLetterLog> SuppressProbe<T> {
    // Called via method resolution (inherent over trait), not by name — suppress false positive.
    #[allow(dead_code)]
    fn value(&self) -> bool {
        true
    }
}

// Low priority: trait method, always available
trait FallbackValue {
    fn value(&self) -> bool;
}

impl<T> FallbackValue for SuppressProbe<T> {
    fn value(&self) -> bool {
        false
    }
}

/// Returns `true` when type `M` implements `SuppressDeadLetterLog`.
/// Must be called at a monomorphisation site where `M` is concrete.
pub(crate) fn suppress_log<M>() -> bool {
    SuppressProbe::<M>(PhantomData).value()
}

/// A message that could not be delivered to its intended recipient.
pub struct DeadLetter {
    pub destination: Pid,
    pub message: Boxed,
    pub sender: Option<Pid>,
}

