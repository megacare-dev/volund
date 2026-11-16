//! Völund: an agent harness for Physical-Engineering's Last Exam. See DESIGN.md.

pub mod agent;
pub mod link;
pub mod server;
pub mod tools;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
