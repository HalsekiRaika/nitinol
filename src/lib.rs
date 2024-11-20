pub use nitinol_core::event::Event;
pub use nitinol_core::command::Command;

#[cfg(feature = "protocol")]
pub mod protocol {
    pub use nitinol_protocol::Payload;
    pub use nitinol_protocol::io;
    pub use nitinol_protocol::adapter;
}

#[cfg(feature = "agent")]
pub mod agent {
    pub use nitinol_agent::any;
    pub use nitinol_agent::Ref;
    pub use nitinol_agent::Context;
    pub use nitinol_agent::{Applicator, Publisher};
}

#[cfg(feature = "projection")]
pub mod projection {
    pub use nitinol_projection::Projector;
}

pub mod errors {
    pub use nitinol_core::errors::*;
    pub use nitinol_agent::errors::*;
    pub use nitinol_protocol::errors::*;
    pub use nitinol_projection::errors::*;
}