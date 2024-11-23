pub use nitinol_core::event::Event;
pub use nitinol_core::command::Command;
pub use nitinol_core::resolver::{ResolveMapping, Mapper};

#[cfg(feature = "protocol")]
pub mod protocol {
    pub use nitinol_protocol::Payload;
    pub use nitinol_protocol::io;
    pub use nitinol_protocol::adapter;
}

#[cfg(feature = "process")]
pub mod process {
    pub use nitinol_process::any;
    pub use nitinol_process::Ref;
    pub use nitinol_process::Context;
    pub use nitinol_process::Process;
    pub use nitinol_process::{Applicator, Publisher};
}

#[cfg(feature = "projection")]
pub mod projection {
    pub use nitinol_core::projection::*;
    pub use nitinol_projection::Projector;
}

pub mod errors {
    pub use nitinol_core::errors::*;
    pub use nitinol_process::errors::*;
    pub use nitinol_protocol::errors::*;
    pub use nitinol_projection::errors::*;
}