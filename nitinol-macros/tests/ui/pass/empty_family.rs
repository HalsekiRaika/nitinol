//! An explicitly empty family (`family = ""`) is a valid, intentional value —
//! it must compile, mirroring the hand-written `Family::new("")` pattern.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "", codec = IncrementedCodec)]
struct Incremented;

struct IncrementedCodec;

impl Codec<Incremented> for IncrementedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Incremented) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Incremented, Self::Error> {
        Ok(Incremented)
    }
}

fn main() {
    let _ = Incremented::EVENT_TYPE;
}
