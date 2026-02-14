pub use nitinol_core::identifier::*;
pub use nitinol_core::event::Event;
pub use nitinol_core::command::Command;

#[cfg(feature = "macro")]
pub use self::macros::*;

#[cfg(feature = "macro")]
mod macros {
    pub use nitinol_macro::Event;
    pub use nitinol_macro::Command;
}

pub mod setup {
    #[cfg(feature = "eventstream")]
    pub use nitinol_eventstream::init_eventstream;
    
    #[cfg(feature = "persistence")]
    pub use nitinol_persistence::set_writer;
    
    #[cfg(feature = "projection")]
    pub use nitinol_projection::set_global_projector;
}

#[cfg(feature = "eventstream")]
pub mod eventstream {
    pub use nitinol_eventstream::eventstream::EventStream;
    pub use nitinol_eventstream::resolver;
    pub use nitinol_eventstream::subscriber::EventSubscriber;
}

#[cfg(feature = "protocol")]
pub mod protocol {
    pub use nitinol_protocol::Payload;
    pub use nitinol_protocol::io;
}

#[cfg(feature = "process")]
pub mod process {
    pub use nitinol_process::any;
    pub use nitinol_process::manager;
    pub use nitinol_process::Receptor;
    pub use nitinol_process::Context;
    pub use nitinol_process::Process;
    pub use nitinol_process::task::{EventApplicator, CommandHandler};
    
    #[cfg(feature = "persistence")]
    pub mod persistence {
        pub use nitinol_persistence::process::*;
        pub use nitinol_persistence::writer;
    }
    
    #[cfg(feature = "eventstream")]
    pub mod eventstream {
        pub use nitinol_eventstream::process::WithStreamPublisher;
        pub use nitinol_eventstream::process::WithEventSubscriber;
    }
}

#[cfg(any(feature = "projection", feature = "eventstream"))]
pub mod resolver {
    pub use nitinol_resolver::*;
}

#[cfg(feature = "projection")]
pub mod projection {
    pub use nitinol_projection::projection::*;
    pub use nitinol_projection::projector;
    pub use nitinol_projection::resolver;
}

pub mod errors {
    pub use nitinol_core::errors::*;
    
    #[cfg(feature = "process")]
    pub use nitinol_process::errors::*;
    
    #[cfg(feature = "protocol")]
    pub use nitinol_protocol::errors::*;
    
    #[cfg(feature = "projection")]
    pub use nitinol_projection::errors::*;
}
