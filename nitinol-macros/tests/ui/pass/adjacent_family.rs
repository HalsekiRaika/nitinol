//! The reserved-namespace check is bounded by path segments, exactly as
//! `MaterializedPath::is_within` is. A family that merely starts with the same
//! letters as the reserved root is an ordinary user family and must compile —
//! a `starts_with("nitinol")` check would confiscate it.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "nitinolx", codec = AdjacentCodec)]
struct Adjacent;

struct AdjacentCodec;

impl Codec<Adjacent> for AdjacentCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Adjacent) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Adjacent, Self::Error> {
        Ok(Adjacent)
    }
}

fn main() {
    let _ = Adjacent::EVENT_TYPE;
}
