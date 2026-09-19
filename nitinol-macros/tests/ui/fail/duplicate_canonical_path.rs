//! One canonical path, claimed by two declarations of one crate.
//!
//! A declaration is written down as one path, and a row carrying that path is
//! restorable as one type. Both types below are written down as
//! `collision.intra.Duplicate`: the module a type is defined in is no part of
//! its identity, so moving one of them into a module of its own does not give
//! it a path of its own.
//!
//! The derive names a declaration with a symbol taken from its path, so the
//! second of these is a symbol already defined — a compile error at the point
//! the build would otherwise produce a binary whose registry cannot be built.
//! Across crates the same collision is the linker's duplicate-symbol error
//! instead, which no single fixture can stage.

mod first {
    use bytes::Bytes;
    use nitinol::contract::Codec;
    use nitinol::eventsource::Event;

    #[derive(Clone, Event)]
    #[event(family = "collision.intra", codec = DuplicateCodec)]
    pub struct Duplicate;

    pub struct DuplicateCodec;

    impl Codec<Duplicate> for DuplicateCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &Duplicate) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Duplicate, Self::Error> {
            Ok(Duplicate)
        }
    }
}

mod second {
    use bytes::Bytes;
    use nitinol::contract::Codec;
    use nitinol::eventsource::Event;

    #[derive(Clone, Event)]
    #[event(family = "collision.intra", codec = DuplicateCodec)]
    pub struct Duplicate;

    pub struct DuplicateCodec;

    impl Codec<Duplicate> for DuplicateCodec {
        type Error = std::convert::Infallible;

        fn encode(_event: &Duplicate) -> Result<Bytes, Self::Error> {
            Ok(Bytes::new())
        }

        fn decode(_payload: &[u8]) -> Result<Duplicate, Self::Error> {
            Ok(Duplicate)
        }
    }
}

fn main() {
    use nitinol::eventsource::Event;

    let _ = first::Duplicate::EVENT_TYPE;
    let _ = second::Duplicate::EVENT_TYPE;
}
