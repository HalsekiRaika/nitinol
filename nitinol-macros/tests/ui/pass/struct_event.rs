//! A unit struct with an explicit family must derive `Event` cleanly and expose
//! the type-level `EVENT_TYPE` const plus the inherited default `variant()`.
//!
//! `Event` also names the codec that writes the event down, so the attribute
//! carries a `codec = ...` alongside the family and the derive emits the
//! associated type from it.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "shop.orders", codec = PlacedCodec)]
struct Placed;

struct PlacedCodec;

impl Codec<Placed> for PlacedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Placed) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Placed, Self::Error> {
        Ok(Placed)
    }
}

fn main() {
    let _ = Placed::EVENT_TYPE;
    let _ = Placed.variant();
}
