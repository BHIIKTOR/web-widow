//! Opaque pagination cursors shared by the job tools.
//!
//! A cursor is the hex of `<scope>:<id>` where `scope` is the owning job or
//! monitor. Decoding verifies the scope, so a cursor from another job is
//! `not_found` rather than a silent restart at the beginning of the result set.

use crate::core::error::{Result, WidowError};

/// Encode a cursor bound to `scope` and positioned after `id`.
pub fn encode(scope: &str, id: i64) -> String {
 let raw = format!("{scope}:{id}");
 raw.as_bytes()
.iter()
.map(|byte| format!("{byte:02x}"))
.collect()
}

/// Decode a cursor and return its scope and position.
pub fn decode(cursor: &str) -> Result<(String, i64)> {
 let invalid =
 || WidowError::NotFound("cursor is not a cursor this server produced".to_string());
 if !cursor.len().is_multiple_of(2) {
 return Err(invalid());
 }
 let bytes: std::result::Result<Vec<u8>, ()> = (0..cursor.len())
.step_by(2)
.map(|index| {
 cursor
.get(index..index + 2)
.and_then(|pair| u8::from_str_radix(pair, 16).ok())
.ok_or(())
 })
.collect();
 let text = String::from_utf8(bytes.map_err(|()| invalid())?).map_err(|_| invalid())?;
 let (scope, id) = text.rsplit_once(':').ok_or_else(invalid)?;
 let id: i64 = id.parse().map_err(|_| invalid())?;
 Ok((scope.to_string(), id))
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn round_trips_and_rejects_junk() {
 let cursor = encode("0192f3aa-bbcc-7ddd-8eee-ffff00001111", 42);
 let (scope, id) = decode(&cursor).unwrap();
 assert_eq!(scope, "0192f3aa-bbcc-7ddd-8eee-ffff00001111");
 assert_eq!(id, 42);
 assert_eq!(decode("zz").unwrap_err().code(), crate::ErrorCode::NotFound);
 assert_eq!(
 decode("6e6f636f6c6f6e").unwrap_err().code(),
 crate::ErrorCode::NotFound,
 "hex without a colon is not a cursor"
);
 assert_eq!(
 decode("abc").unwrap_err().code(),
 crate::ErrorCode::NotFound
);
 }
}
