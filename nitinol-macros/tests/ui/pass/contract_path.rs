//! A downstream crate must be able to reach the `Event` trait, the `Event`
//! derive and the `Codec` the derive now demands through `nitinol::contract`,
//! the path the generated code names — so the derive is usable without
//! importing `nitinol::eventsource`.

use bytes::Bytes;
use nitinol::contract::{Codec, Event};

#[derive(Clone, Event)]
#[event(family = "shop.orders", codec = ShippedCodec)]
struct Shipped;

struct ShippedCodec;

impl Codec<Shipped> for ShippedCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Shipped) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Shipped, Self::Error> {
        Ok(Shipped)
    }
}

fn main() {
    let _ = Shipped::EVENT_TYPE;
}
