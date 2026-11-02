//! Pure domain layer: no I/O, no config, no network. Everything here is
//! testable with a table of inputs.

pub mod cursor;
pub mod error;
pub mod ip;
pub mod robots;
pub mod token;
pub mod truncate;
pub mod url;

pub use error::{ErrorCode, Result, ToolError, WidowError};
pub use robots::RobotsDecision;
pub use url::PageUrl;
