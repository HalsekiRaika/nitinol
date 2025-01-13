use serde::{Deserialize, Serialize};
use nitinol_macro::Event;

#[derive(Event, Deserialize, Serialize)]
#[persist(
    enc = "serde_json::to_vec",
    dec = "serde_json::from_slice"
)]
pub enum DomainEvent {
    Event1,
    Event2,
    Event3,
}

#[derive(Event, Deserialize, Serialize)]
#[persist(
    key = "another_key",
    enc = "serde_json::to_vec",
    dec = "serde_json::from_slice"
)]
pub enum AnotherEvent {
    Event1,
    Event2,
    Event3,
}