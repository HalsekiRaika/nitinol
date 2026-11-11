//! A small process runtime built on Tokio.
//!
//! A process is a value whose type implements [`Process`](process::Process).
//! It owns its state, runs on its own Tokio task and handles one message at a
//! time. Describe a process with [`Props`](struct@Props), start it with
//! [`ProcessSystem::spawn`], and talk to it through the
//! [`ProcessProxy`](process::ProcessProxy) that `spawn` returns:
//! [`tell`](process::ProcessProxy::tell) sends a message, and
//! [`ask`](process::ProcessProxy::ask) sends one and waits for the reply. On
//! top of this the runtime provides supervision, watching, a name registry,
//! publish/subscribe through [`Stream`], and dead letters.
//!
//! # Getting started
//!
//! A counter that adds one on every `Increment` and replies with its new
//! value:
//!
//! ```
//! use std::convert::Infallible;
//!
//! use nitinol_runtime::process::{Process, ProcessContext, Receive};
//! use nitinol_runtime::{ProcessSystem, Props};
//!
//! struct Counter {
//!     count: u32,
//! }
//!
//! impl Process for Counter {}
//!
//! struct Increment;
//!
//! impl Receive<Increment> for Counter {
//!     type Response = u32;
//!     type Error = Infallible;
//!
//!     async fn recv(
//!         &mut self,
//!         _msg: Increment,
//!         _ctx: &mut ProcessContext<Self>,
//!     ) -> Result<u32, Infallible> {
//!         self.count += 1;
//!         Ok(self.count)
//!     }
//! }
//!
//! #[tokio::main]
//! async fn main() {
//!     let system = ProcessSystem::new().await;
//!     let counter = system.spawn(Props::new(|| Counter { count: 0 })).await;
//!
//!     let count = counter.ask(Increment).await.expect("ask failed");
//!     assert_eq!(count, 1);
//!
//!     counter.stop().await.expect("stop failed");
//! }
//! ```
//!
//! [`Props::new`] takes a factory rather than a value, so that a restarted
//! process can start again from a fresh `Counter`.
//!
//! # Key types
//!
//! - [`ProcessSystem`] hosts processes: it spawns them and finds them by pid
//!   or by name.
//! - [`Props`](struct@Props) describes how to start a process: its factory,
//!   and optionally its name, supervision strategy, idle timeout and
//!   capacities.
//! - [`Process`](process::Process) is implemented by the state of a process,
//!   and has optional lifecycle hooks.
//! - [`Receive<M>`](process::Receive) handles messages of type `M` and defines
//!   the reply that `ask` returns.
//! - [`ProcessProxy<P>`](process::ProcessProxy) is the handle to a running
//!   process, used to `tell`, `ask` and `stop` it.
//! - [`ProcessContext<P>`](process::ProcessContext) is passed to every handler
//!   and hook. It gives the process its own pid and proxy, and lets it spawn
//!   children, watch other processes and stop itself.
//!
//! The errors these operations return are in [`error`], and process
//! identifiers are in [`ident`].
//!
//! # Features
//!
//! **Supervision.** When a handler returns `Err`, the process's
//! [`SupervisionStrategy`] decides what happens next: stop the process (the
//! default), resume with the next message, or restart it from its factory, at
//! most as often as its [`RestartConfig`] allows. Set it with
//! [`Props::with_supervision_strategy`]. See the
//! [`supervision`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime/5.supervision)
//! example.
//!
//! **Watching.** A process calls
//! [`ProcessContext::watch`](process::ProcessContext::watch) with another
//! process's pid to learn when that process ends: its
//! [`Process::on_terminated`](process::Process::on_terminated) hook then
//! receives a [`Terminated`] with the pid and the [`TerminatedReason`]. See the
//! [`watching`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime/4.watching)
//! example.
//!
//! **Registry.** Name a process with [`Props::with_name`] and find it later
//! with [`ProcessSystem::lookup_by_name`]. The lookup returns an
//! [`AnyProxy`](process::AnyProxy), which
//! [`downcast`](process::AnyProxy::downcast) turns back into a typed
//! `ProcessProxy`. See the
//! [`registry`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime/3.registry)
//! example.
//!
//! **Streams.** A [`Stream`] is a built-in process that broadcasts every
//! published message to its subscribers. Start one per topic with
//! `system.spawn(StreamProps::new(topic))`, then `subscribe` processes to it
//! and `publish` to all of them. A subscriber is any process that receives
//! the stream's message type; implement [`Subscriber`] and spawn it with
//! [`Props::subscriber`] for one that only consumes messages. See the
//! [`streams`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime/6.streams)
//! example.
//!
//! **Dead letters.** A message that cannot be delivered, for example because
//! its target has stopped, becomes a [`DeadLetter`]. Subscribe to the
//! [`DeadLetterStream`] from [`ProcessSystem::dead_letter_stream`] to receive
//! them. See the
//! [`dead-letters`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime/7.dead-letters)
//! example.
//!
//! **Idle timeout.** An [`IdleTimeout`] stops a process that has received no
//! message for a given time. Set it per process with
//! [`Props::with_idle_timeout`], or for every process with
//! [`ProcessSystemBuilder::with_default_idle_timeout`]. Watchers of a process
//! stopped this way see [`TerminatedReason::Timeout`].
//!
//! # Examples
//!
//! Complete programs are in
//! [`examples/runtime`](https://github.com/HalsekiRaika/nitinol/tree/master/examples/runtime)
//! in the repository. Each is a workspace package; run it from a clone with
//! `cargo run -p <package>`.
//!
//! - `cargo run -p basic-process`: spawn a process and stop it.
//! - `cargo run -p messaging`: `tell` and `ask`; the full version of the
//!   counter above.
//! - `cargo run -p registry`: name a process and look it up by name.
//! - `cargo run -p watching`: watch a process and receive [`Terminated`].
//! - `cargo run -p supervision`: the `Stop` and `Restart` strategies.
//! - `cargo run -p streams`: publish/subscribe with a [`Stream`].
//! - `cargo run -p dead-letters`: receive undeliverable messages from the
//!   dead-letter stream.
//!
//! # Related crates
//!
//! This crate is the runtime layer of [`nitinol`](https://docs.rs/nitinol),
//! which re-exports it as `nitinol::runtime` and adds layers for persistence,
//! event sourcing and sagas on top of it.

pub mod error;
pub mod ident;
pub mod process;
mod system;

pub use self::process::{
    BoxedMessage, DeadLetter, DefaultDriver, IdleTimeout, MailboxCapacity, Message, PidSet,
    PipeCapacity, Props, Resident, RestartConfig, StashCapacity, Stream, StreamProps, Subscriber,
    SupervisionStrategy, SuppressDeadLetterLog, Terminated, TerminatedReason,
};
pub use self::system::{DeadLetterStream, ProcessSystem, ProcessSystemBuilder};
