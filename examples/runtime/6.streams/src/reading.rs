//! Temperature reading message type for the streams example.
//!
//! # Why Clone?
//! `Stream<BoxedMessage>` broadcasts one value to every subscriber. Internally,
//! `publish` wraps the message in a `BoxedMessage` (`Arc<dyn Any + Send + Sync>`)
//! and calls `msg.clone()` for each subscriber (see `stream.rs`, `T: Clone`
//! constraint on `Receive<PublishMsg<T>>`).  The cloning of `BoxedMessage` is
//! inexpensive — only the reference count increments — but the compile-time `Clone`
//! requirement on the type-erased inner value is what forces `TemperatureReading`
//! to derive `Clone`.  Without it the type cannot be published to a multi-subscriber stream.

/// A temperature measurement published by a sensor to the stream.
///
/// Derives `Clone` so the stream can fan-out one published value to an
/// arbitrary number of subscribers without deep-copying: `BoxedMessage::new`
/// wraps the value in `Arc`, and each subscriber receives a clone of that `Arc`.
#[derive(Debug, Clone)]
pub struct TemperatureReading {
    /// Identifier of the sensor that produced this reading.
    pub sensor: String,
    /// Temperature in degrees Celsius.
    pub celsius: f64,
}
