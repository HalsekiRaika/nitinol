//! Process identifiers.
//!
//! Every process gets a [`Pid`] when it is spawned. Read it from
//! [`ProcessProxy::pid`](crate::process::ProcessProxy::pid) or
//! [`ProcessContext::pid`](crate::process::ProcessContext::pid), and use it to
//! [`watch`](crate::process::ProcessContext::watch) a process or to find it
//! with [`ProcessSystem::lookup`](crate::ProcessSystem::lookup).
//!
//! A [`ProcessName`] is an optional name you choose. Give it to a process with
//! [`Props::with_name`](crate::Props::with_name), and find the process by it
//! with [`ProcessSystem::lookup_by_name`](crate::ProcessSystem::lookup_by_name).
//!
//! ```
//! use nitinol_runtime::ident::ProcessName;
//! use nitinol_runtime::process::Process;
//! use nitinol_runtime::{ProcessSystem, Props};
//!
//! struct Worker;
//!
//! impl Process for Worker {}
//!
//! #[tokio::main]
//! async fn main() {
//!     let system = ProcessSystem::new().await;
//!     let name = ProcessName::new("worker");
//!     let worker = system
//!         .spawn(Props::new(|| Worker).with_name(name.clone()))
//!         .await;
//!
//!     let by_pid = system.lookup(worker.pid()).await.expect("no process for pid");
//!     assert!(by_pid.downcast::<Worker>().is_some());
//!
//!     let by_name = system.lookup_by_name(&name).await.expect("no process for name");
//!     assert_eq!(by_name.downcast::<Worker>().unwrap().pid(), worker.pid());
//!
//!     worker.stop().await.expect("stop failed");
//! }
//! ```

mod name;
mod pid;

pub use self::{name::*, pid::*};
