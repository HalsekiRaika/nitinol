use bytes::Bytes;

/// How a domain value is written to, and read back from, the bytes a store holds.
///
/// Both methods are associated functions (no `&self`) — a codec is a format, not
/// a value, so it is reached through its type and never constructed.
///
/// A codec lives here, alongside [`Event`](crate::Event) and
/// [`Snapshotable`](crate::Snapshotable), because those traits *name* one:
/// `Event::Codec` says which format writes the event down, and
/// `Snapshotable::Codec` says which format writes its snapshot. Naming a codec
/// is therefore part of defining a domain type, and must be expressible with
/// this crate alone — without the execution machinery, or an async runtime, that
/// later reads the association.
///
/// The framework ships no implementation of this trait: which wire format a
/// domain uses is the domain's decision, and supplying one here would put a
/// serializer in the dependency graph of every crate that merely defines an
/// event.
pub trait Codec<T>: Send + Sync + 'static {
    /// The error type produced when encoding or decoding fails.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Encode `value` to raw bytes.
    fn encode(value: &T) -> Result<Bytes, Self::Error>;

    /// Decode a value from raw bytes.
    fn decode(payload: &[u8]) -> Result<T, Self::Error>;
}
