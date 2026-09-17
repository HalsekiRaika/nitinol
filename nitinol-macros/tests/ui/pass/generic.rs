//! Generic types whose parameters satisfy the `Event` supertrait bounds must
//! derive cleanly; `split_for_impl` threads type params and where-clauses.
//!
//! The derive copies the named codec into `type Codec = ..` verbatim and infers
//! no bounds for it, so making one codec serve `Wrapper<T>` for every `T` is
//! the fixture's job: `WrapperCodec` needs a way to produce a `T` when it
//! decodes, and `T: Default` is what supplies it. A derive that tried to infer
//! this would be doing type inference rather than attribute parsing.

use bytes::Bytes;
use nitinol::contract::Codec;
use nitinol::eventsource::Event;

#[derive(Clone, Event)]
#[event(family = "generic", codec = WrapperCodec)]
struct Wrapper<T: Clone + Default + Send + Sync + 'static> {
    inner: T,
}

struct WrapperCodec;

impl<T: Clone + Default + Send + Sync + 'static> Codec<Wrapper<T>> for WrapperCodec {
    type Error = std::convert::Infallible;

    fn encode(_event: &Wrapper<T>) -> Result<Bytes, Self::Error> {
        Ok(Bytes::new())
    }

    fn decode(_payload: &[u8]) -> Result<Wrapper<T>, Self::Error> {
        Ok(Wrapper { inner: T::default() })
    }
}

fn main() {
    let _ = <Wrapper<u32> as Event>::EVENT_TYPE;
    let _ = Wrapper { inner: 1u32 }.variant();
}
