//! A declared source is resolved to one event type at declaration time — that
//! is what the decode branch and the binding slot are built from. A generic
//! alphabet leaves the set of sources open until instantiation, so neither can
//! be generated.

use nitinol::saga::SagaSubscribe;

struct Placed;

#[derive(SagaSubscribe)]
enum Observed<T> {
    Placed(Placed),
    Other(T),
}

fn main() {}
