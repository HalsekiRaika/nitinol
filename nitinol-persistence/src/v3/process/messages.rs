use nitinol_core::event::Event;
use nitinol_core::identifier::EntityId;
use nitinol_process::message::Message;

impl<E: Event> Message for WriteEvent<E> {}

#[derive(Debug)]
pub(crate) struct WriteEvent<E: Event> {
    from: EntityId,
    version: i64,
    event: E,
}
