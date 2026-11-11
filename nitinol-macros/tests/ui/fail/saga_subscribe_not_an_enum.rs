//! An alphabet is a choice between inputs, so `#[derive(SagaSubscribe)]`
//! supports enums only: a struct names one shape, not a set of them.

use nitinol::saga::SagaSubscribe;

#[derive(SagaSubscribe)]
struct Bad;

fn main() {}
