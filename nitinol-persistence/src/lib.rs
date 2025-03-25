pub mod process;
pub mod writer;

mod global;

pub use self::global::set_writer;

#[cfg(feature = "global")]
pub use self::global::get_global_writer;