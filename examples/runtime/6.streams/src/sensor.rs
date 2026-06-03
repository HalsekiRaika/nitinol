//! Temperature sensor process for the streams example.
//!
//! `TemperatureSensor` receives `Measure(f64)` commands, constructs a
//! `TemperatureReading`, and publishes it to a `Stream<BoxedMessage>`.
//! The stream then fans the reading out to every registered subscriber.

use nitinol_runtime::error::SendError;
use nitinol_runtime::process::{Process, ProcessContext, ProcessProxy, Receive};
use nitinol_runtime::{BoxedMessage, Stream};

use crate::reading::TemperatureReading;

// ─── Message ─────────────────────────────────────────────────────────────────

/// Command: record a temperature measurement of `celsius` degrees.
pub struct Measure(pub f64);

// ─── Process ─────────────────────────────────────────────────────────────────

/// A sensor that converts `Measure` commands into `TemperatureReading`
/// publications on the shared stream.
pub struct TemperatureSensor {
    name: String,
    stream: ProcessProxy<Stream<BoxedMessage>>,
}

impl TemperatureSensor {
    pub fn new(name: String, stream: ProcessProxy<Stream<BoxedMessage>>) -> Self {
        Self { name, stream }
    }
}

impl Process for TemperatureSensor {}

// ─── Receive<Measure> ────────────────────────────────────────────────────────

impl Receive<Measure> for TemperatureSensor {
    type Response = ();
    type Error = SendError;

    async fn recv(
        &mut self,
        msg: Measure,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), SendError> {
        let reading = TemperatureReading {
            sensor: self.name.clone(),
            celsius: msg.0,
        };
        self.stream.publish_boxed(reading).await
    }
}
