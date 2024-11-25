use nitinol_protocol::io::{ReadProtocol, WriteProtocol};

#[derive(Clone)]
pub struct EventStore {
    writer: WriteProtocol,
    reader: ReadProtocol
}

