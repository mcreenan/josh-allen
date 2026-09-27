#![forbid(unsafe_code)]

//! Reference session state and stdio adapter for the current JOSH protocol.

mod check;
mod events;
mod grants;
mod origin;
mod server;
mod session;

pub use check::{ProgramError, compile_source_bundle, compiler_diagnostics};
pub use origin::{ProgramDebug, readable_function_name};
pub use server::run_connection;
pub use session::{HostError, PreparedExecution, Session};
