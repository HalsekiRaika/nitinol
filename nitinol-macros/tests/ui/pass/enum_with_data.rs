//! Enums mixing unit, tuple, and struct arms must derive `Event`; the generated
//! `variant()` match ignores payload fields via `(..)` and `{ .. }`.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "domain", codec = MixedCodec)]
enum Mixed {
    Unit,
    Tuple(u32, String),
    Struct { id: u64, name: String },
}

struct MixedCodec;

impl Codec<Mixed> for MixedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Mixed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Mixed, Self::Error> {
        Ok(Mixed::Unit)
    }
}

fn main() {
    let _ = Mixed::EVENT_TYPE;
    let _ = Mixed::Unit.variant();
    let _ = Mixed::Tuple(1, String::new()).variant();
    let _ = Mixed::Struct { id: 2, name: String::new() }.variant();
}
