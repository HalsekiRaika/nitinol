//! Shared context-wiring helpers.
//!
//! Both [`crate::process::ProcessContext`] and
//! [`crate::process::SubscriberContext`] expose `watch`, `unwatch`, and
//! `stop_self` with identical registry-lookup / system-signal / dead-letter
//! logic.  This module holds the single implementation; both context types
//! delegate to it so that a change to watch semantics only needs to land once.

use std::any::TypeId;

use tokio::sync::mpsc;

use crate::error::SendError;
use crate::ident::Pid;
use crate::process::dead_letter::{DeadLetterEnvelope, DeadLetterProxy};
use crate::process::message::BoxedMessage;
use crate::process::registry::ProcessRegistry;
use crate::process::signal::SystemSignal;
use crate::process::watch::{TerminatedReason, WatchRequest};

pub(super) async fn watch(
    watcher_pid: Pid,
    target_pid: Pid,
    registry: &ProcessRegistry,
    sys_tx: &mpsc::Sender<SystemSignal>,
    dead_letter: Option<&DeadLetterProxy>,
) {
    match registry.lookup(target_pid).await {
        Some(proxy) => {
            let result = proxy
                .send_system_signal(SystemSignal::Watch { watcher_pid })
                .await;
            if result.is_err() {
                // Target stopped between lookup and signal delivery.
                deliver_not_found(watcher_pid, target_pid, sys_tx, dead_letter).await;
            }
        }
        None => {
            deliver_not_found(watcher_pid, target_pid, sys_tx, dead_letter).await;
        }
    }
}

pub(super) async fn unwatch(watcher_pid: Pid, target_pid: Pid, registry: &ProcessRegistry) {
    if let Some(proxy) = registry.lookup(target_pid).await {
        let _ = proxy
            .send_system_signal(SystemSignal::Unwatch { watcher_pid })
            .await;
    }
}

pub(super) async fn stop_self(sys_tx: &mpsc::Sender<SystemSignal>) -> Result<(), SendError> {
    sys_tx.send(SystemSignal::Stop).await.map_err(|_| SendError)
}

/// Routing through dead-letter ensures the `Terminated { why: NotFound }`
/// signal arrives via the same watch-contract path as a normal termination,
/// keeping the watcher's handling logic uniform.
async fn deliver_not_found(
    watcher_pid: Pid,
    target_pid: Pid,
    sys_tx: &mpsc::Sender<SystemSignal>,
    dead_letter: Option<&DeadLetterProxy>,
) {
    match dead_letter {
        Some(dl) => {
            let envelope = DeadLetterEnvelope {
                destination: target_pid,
                message: BoxedMessage::new(WatchRequest {
                    watched: target_pid,
                    watcher: watcher_pid,
                }),
                sender: Some(watcher_pid),
                suppress_log: true,
                message_type_id: TypeId::of::<WatchRequest>(),
            };
            dl.send(envelope).await;
        }
        None => {
            let _ = sys_tx
                .send(SystemSignal::Terminated {
                    who: target_pid,
                    why: TerminatedReason::NotFound,
                })
                .await;
        }
    }
}
