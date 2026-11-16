//! Völund: an agent harness for Physical-Engineering's Last Exam. See DESIGN.md.

pub mod ablation;
pub mod agent;
pub mod driver;
pub mod link;
pub mod mcp;
pub mod server;
pub mod tools;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
