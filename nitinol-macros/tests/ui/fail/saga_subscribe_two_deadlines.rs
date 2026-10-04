//! A saga has one clock. Two `#[subscribe(deadline)]` variants would give the
//! schedule fact two possible readings for the same bytes, with nothing on the
//! wire to say which arm a firing belongs to.

use nitinol::saga::SagaSubscribe;

struct Placed;
struct First;
struct Second;

#[derive(SagaSubscribe)]
enum Observed {
    Placed(Placed),
    #[subscribe(deadline)]
    First(First),
    #[subscribe(deadline)]
    Second(Second),
}

fn main() {}
