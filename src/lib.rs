pub use spectroscopy_core::event::Event;
pub use spectroscopy_core::command::Command;

#[cfg(feature = "protocol")]
pub mod protocol {
    pub use spectroscopy_protocol::Payload;
    pub use spectroscopy_protocol::io;
    pub use spectroscopy_protocol::adapter;
}

#[cfg(feature = "agent")]
pub mod agent {
    pub use spectroscopy_agent::any;
    pub use spectroscopy_agent::Ref;
    pub use spectroscopy_agent::{Applicator, Publisher};
}

#[cfg(feature = "projection")]
pub mod projection {
    pub use spectroscopy_projection::Projector;
}
