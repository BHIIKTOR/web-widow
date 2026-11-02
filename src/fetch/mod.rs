//! Network adapter layer: the only module that talks HTTP, plus the pure policy
//! it consults (content scope, charset, per-host politeness).

pub mod charset;
pub mod content;
pub mod gate;
pub mod http;
pub mod robots;
pub mod ssrf;
