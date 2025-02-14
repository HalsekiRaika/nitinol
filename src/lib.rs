pub use nitinol_core::identifier::*;
pub use nitinol_core::event::Event;
pub use nitinol_core::command::Command;

#[cfg(feature = "macro")]
pub mod macros {
    pub use nitinol_macro::Event;
    pub use nitinol_macro::Command;
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
    pub use nitinol_protocol::adapter;
}

#[cfg(feature = "process")]
pub mod process {
    pub use nitinol_process::any;
    pub use nitinol_process::manager;
    pub use nitinol_process::Ref;
    pub use nitinol_process::Context;
    pub use nitinol_process::Process;
    pub use nitinol_process::{Applicator, Publisher, TryApplicator};

    #[cfg(feature = "process-ext")]
    pub mod extension {
        pub use nitinol_process::extension::*;
        pub use nitinol_process::FromContextExt;
    }
    
    #[cfg(feature = "persistence")]
    pub mod persistence {
        pub use nitinol_persistence::process::*;
        pub use nitinol_persistence::extension::PersistenceExtension;
    }
    
    #[cfg(feature = "eventstream")]
    pub mod eventstream {
        pub use nitinol_eventstream::extension::EventStreamExtension;
        pub use nitinol_eventstream::extension::WithStreamPublisher;
        pub use nitinol_eventstream::extension::WithEventSubscriber;
    }
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
