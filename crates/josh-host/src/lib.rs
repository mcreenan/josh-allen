#![forbid(unsafe_code)]

//! Reference session state and stdio adapter for the current JOSH protocol.

mod events;
mod grants;
mod server;
mod session;

pub use server::run_connection;
pub use session::{HostError, PreparedExecution, Session};
