//! Shared test helpers. Each test binary compiles this module in full but
//! uses only the subset it needs, so per-binary dead code is expected.

#![allow(dead_code)]

use nitinol_eventsource::Effect;
use nitinol_runtime::process::{Process, ProcessContext, Receive};

// Minimal dummy process used by tell() tests.

pub struct TestProcess;

pub struct TestMsg;

impl Process for TestProcess {}

impl Receive<TestMsg> for TestProcess {
    type Response = ();
    type Error = std::convert::Infallible;

    async fn recv(
        &mut self,
        _msg: TestMsg,
        _ctx: &mut ProcessContext<Self>,
    ) -> Result<(), std::convert::Infallible> {
        Ok(())
    }
}

// Shape — a PartialEq + Debug mirror of Effect<E> used for structural
// comparison without requiring Effect itself to implement PartialEq or Debug.
// The Side variant does not carry data because Box<dyn SideEffect> is opaque.

#[derive(Debug, PartialEq)]
pub enum Shape<E> {
    None,
    Persist(Vec<E>),
    Apply(Vec<E>),
    Side,
    Sequence(Vec<Shape<E>>),
}

pub fn shape_of<E: Clone>(effect: &Effect<E>) -> Shape<E> {
    match effect {
        Effect::None => Shape::None,
        Effect::Persist(events) => Shape::Persist(events.clone()),
        Effect::Apply(events) => Shape::Apply(events.clone()),
        Effect::Side(_) => Shape::Side,
        Effect::Sequence(children) => Shape::Sequence(children.iter().map(shape_of).collect()),
    }
}
