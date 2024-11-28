#![allow(clippy::type_complexity)]

pub mod errors;

use nitinol_process::extension::Extensions;
use nitinol_core::identifier::{EntityId, ToEntityId};
use nitinol_process::registry::{ProcessSystem, Registry};
use nitinol_process::Context as ProcessContext;
use nitinol_process::{Process, Ref};
use pin_project::pin_project;
use std::convert::Infallible;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub trait Executable<H> {
    type Accept;
    type Rejection;
    type Future: Future<Output = Result<Self::Accept, Self::Rejection>>;
    fn call(&mut self, resource: H) -> Self::Future;
}

#[cfg(test)]
mod test {
    use async_trait::async_trait;
    use super::*;
    use tokio::time::Instant;
    use nitinol_core::command::Command;
    use nitinol_core::errors::{DeserializeError, SerializeError};
    use nitinol_core::event::Event;
    use nitinol_core::resolver::{Mapper, ResolveMapping};
    use nitinol_process::extension::Extensions;
    use nitinol_process::Publisher;
    use nitinol_process::registry::Registry;

    #[derive(Clone)]
    pub struct TestProcess {
        a: String,
        b: String,
        c: String,
        d: String,
    }
    
    pub struct TestCommand;
    impl Command for TestCommand {}
    pub struct TestEvent;
    impl Event for TestEvent {
        const REGISTRY_KEY: &'static str = "test-event";

        fn as_bytes(&self) -> Result<Vec<u8>, SerializeError> {
            unimplemented!()
        }

        fn from_bytes(bytes: &[u8]) -> Result<Self, DeserializeError> {
            unimplemented!()
        }
    }

    impl ResolveMapping for TestProcess {
        fn mapping(_: &mut Mapper<Self>) {
            // No-op ;D
        }
    }
    impl Process for TestProcess {}
    
    #[async_trait]
    impl Publisher<TestCommand> for TestProcess {
        type Event = TestEvent;
        type Rejection = Infallible;

        async fn publish(&self, _: TestCommand, _: &mut ProcessContext) -> Result<Self::Event, Self::Rejection> {
            println!("aaaa");
            Ok(TestEvent)
        }
    }

    #[tokio::test]
    async fn execute() {
        let registry = Registry::default();
        let extensions = Extensions::builder().build();
        let executor = Executor::new(registry, extensions);
        
        let instant = Instant::now();
        let entity = TestProcess {
            a: "aaa".to_string(),
            b: "bbb".to_string(),
            c: "ccc".to_string(),
            d: "ddd".to_string(),
        };
        let a = executor.exec(spawn("a", entity.clone())).await.unwrap();
        let b = executor.exec(spawn("b", entity.clone())).await.unwrap();
        let c = executor.exec(spawn("c", entity.clone())).await.unwrap();
        let d = executor.exec(spawn("d", entity.clone())).await.unwrap();
        let e = executor.exec(spawn("e", entity)).await.unwrap();
        
        let _ = a.publish(TestCommand).await.unwrap();
        let _ = b.publish(TestCommand).await.unwrap();
        let _ = c.publish(TestCommand).await.unwrap();
        let _ = d.publish(TestCommand).await.unwrap();
        let _ = e.publish(TestCommand).await.unwrap();
        
        println!("{}μs", instant.elapsed().as_micros())
    }
}

pub struct Executor {
    registry: Registry,
    extensions: Extensions
}

impl Executor {
    pub fn new(reg: Registry, ext: Extensions) -> Executor {
        Self { registry: reg, extensions: ext }
    }
}

#[derive(Clone)]
pub struct Tracker {
    registry: Registry,
    extensions: Extensions
}

impl Executor {
    pub fn exec<X: Executable<Tracker>>(&self, mut exec: X) -> X::Future {
        exec.call(Tracker {
            registry: self.registry.clone(),
            extensions: self.extensions.clone()
        })
    }
}

pub fn spawn<T: Process + Clone>(id: impl ToEntityId, entity: T) -> Spawner<Transparent<T>> {
    Spawner { id: id.to_entity_id(), seq: 0, inner_handler: Transparent(entity) }
}

pub fn spawn_with<X: Executable<Tracker>>(id: impl ToEntityId, exec: X) -> Spawner<X> {
    Spawner { id: id.to_entity_id(), seq: 0, inner_handler: exec }
}

pub struct Transparent<T>(T);

impl<T: Clone + 'static, R> Executable<R> for Transparent<T> {
    type Accept = T;
    type Rejection = Infallible;
    type Future = future::Transparent<T>;

    fn call(&mut self, _: R) -> Self::Future {
        future::Transparent(self.0.clone())
    }
}

mod future {
    use std::convert::Infallible;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    pub struct Transparent<T: Clone>(pub T);
    
    impl<T: Clone> Future for Transparent<T> {
        type Output = Result<T, Infallible>;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(self.0.clone()))
        }
    }
}

pub struct Spawner<H> {
    id: EntityId,
    seq: i64,
    inner_handler: H
}

impl<T: Process, H> Executable<Tracker> for Spawner<H>
where
    T: Clone,
    H: Executable<Tracker, Accept = T>,
    H::Rejection: Into<Box<dyn Error + Sync + Send>>
{
    type Accept = Ref<H::Accept>;
    type Rejection = Box<dyn Error + Sync + Send>;
    type Future = Spawn<H::Future>;

    fn call(&mut self, tracker: Tracker) -> Self::Future {
        let fut = self.inner_handler.call(tracker.clone());
        Spawn {
            id: self.id.clone(),
            seq: self.seq,
            tracker,
            inner_handler: fut
        }
    }
}

#[pin_project]
pub struct Spawn<H> {
    id: EntityId,
    seq: i64,
    #[pin]
    tracker: Tracker,
    #[pin]
    inner_handler: H
}

impl<F, T: Process, E> Future for Spawn<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<Box<dyn Error + Sync + Send>>
{
    type Output = Result<Ref<T>, Box<dyn Error + Sync + Send>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        
        let handler = this.inner_handler;
        let entity = match handler.poll(cx) {
            Poll::Ready(res) => res.map_err(Into::into)?,
            Poll::Pending => return Poll::Pending
        };

        let tracker = this.tracker;
        let registry = tracker.registry.clone();
        
        match std::pin::pin!(registry.find::<T>(this.id.clone())).poll(cx) {
            Poll::Ready(res) => {
                match res {
                    Ok(Some(refs)) => return Poll::Ready(Ok(refs)),
                    Ok(None) => {}
                    Err(e) => { return Poll::Ready(Err(Box::new(e))) },
                }
            },
            Poll::Pending => {}
        }
        
        let extension = tracker.extensions.clone();
        let context = ProcessContext::new(*this.seq, registry.clone(), extension);
        
        match std::pin::pin!(nitinol_process::lifecycle::run(this.id.clone(), entity, context, registry)).poll(cx) {
            Poll::Ready(res) => return Poll::Ready(res.map_err(Into::into)),
            Poll::Pending => {}
        }

        Poll::Pending
    }
}
