//! Every variant of an alphabet names exactly one input type: that type is what
//! the decode branch reads and what the binding slot is keyed by. A unit variant
//! names none, so there is nothing to subscribe to and nothing to decode.

use nitinol::saga::SagaSubscribe;

struct Placed;

#[derive(SagaSubscribe)]
enum Observed {
    Placed(Placed),
    Bare,
}

fn main() {}
