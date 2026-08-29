use std::any::Any;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use futures_core::future::BoxFuture;
use nitinol_contract::{Aggregate, Decider};
use nitinol_persistence::AggregateId;

use crate::error::TellError;
use crate::process::AggregateTellTarget;

/// The stream key the mock names as its target aggregate.  Fixed and
/// non-empty, so an intent built over the mock satisfies consumers that
/// require a target they could report a failure against.
const MOCK_AGGREGATE_ID: &str = "mock-aggregate-target";

pub struct MockAggregateProxy<A: Aggregate> {
    captured: Arc<Mutex<Vec<Box<dyn Any + Send>>>>,
    aggregate_id: AggregateId,
    _phantom: PhantomData<fn() -> A>,
}

impl<A: Aggregate> MockAggregateProxy<A> {
    pub fn new() -> Self {
        Self {
            captured: Arc::new(Mutex::new(Vec::new())),
            aggregate_id: AggregateId::new(MOCK_AGGREGATE_ID),
            _phantom: PhantomData,
        }
    }

    pub async fn tell<C>(&self, cmd: C) -> Result<(), TellError>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        self.captured
            .lock()
            .expect("MockAggregateProxy capture buffer mutex must not be poisoned")
            .push(Box::new(cmd));
        Ok(())
    }

    pub fn drain_captured<C: 'static>(&self) -> Vec<C> {
        let mut buffer = self
            .captured
            .lock()
            .expect("MockAggregateProxy capture buffer mutex must not be poisoned");

        let mut matched: Vec<C> = Vec::new();
        let mut remaining: Vec<Box<dyn Any + Send>> = Vec::with_capacity(buffer.len());

        for item in buffer.drain(..) {
            match item.downcast::<C>() {
                Ok(boxed) => matched.push(*boxed),
                Err(original) => remaining.push(original),
            }
        }

        *buffer = remaining;
        matched
    }
}

impl<A: Aggregate> Default for MockAggregateProxy<A> {
    fn default() -> Self {
        Self::new()
    }
}

impl<A: Aggregate> Clone for MockAggregateProxy<A> {
    fn clone(&self) -> Self {
        Self {
            captured: Arc::clone(&self.captured),
            aggregate_id: self.aggregate_id.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<A: Aggregate> AggregateTellTarget<A> for MockAggregateProxy<A> {
    fn tell<'a, C>(&'a self, cmd: C) -> BoxFuture<'a, Result<(), TellError>>
    where
        A: Decider<C>,
        C: Send + Sync + 'static,
        <A as Decider<C>>::Rejection: std::error::Error + Send + Sync + 'static,
    {
        Box::pin(MockAggregateProxy::tell(self, cmd))
    }

    fn aggregate_id(&self) -> &AggregateId {
        &self.aggregate_id
    }
}
