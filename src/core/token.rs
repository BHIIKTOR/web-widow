//! Constant-time secret comparison.
//!
//! Used by the MCP bearer middleware and the feed capability token. The length
//! is not secret (a wrong length fails immediately); the bytes are compared
//! without an early exit so a timing signal does not leak the matching prefix.

/// Constant-time byte comparison: `false` on a length mismatch, otherwise every
/// byte is compared.
pub fn constant_time_eq(provided: &[u8], expected: &[u8]) -> bool {
 if provided.len() != expected.len() {
 return false;
 }
 let mut difference = 0u8;
 for (provided, expected) in provided.iter().zip(expected) {
 difference |= provided ^ expected;
 }
 difference == 0
}

/// Constant-time comparison of two strings.
pub fn constant_time_eq_str(provided: &str, expected: &str) -> bool {
 constant_time_eq(provided.as_bytes(), expected.as_bytes())
}

#[cfg(test)]
mod tests {
 use super::*;

 #[test]
 fn comparisons_are_exact() {
 assert!(constant_time_eq(b"secret-token", b"secret-token"));
 assert!(!constant_time_eq(b"secret-toke", b"secret-token"));
 assert!(!constant_time_eq(b"secret-tokeN", b"secret-token"));
 assert!(!constant_time_eq(b"", b"secret-token"));
 assert!(constant_time_eq_str("abc", "abc"));
 assert!(!constant_time_eq_str("abd", "abc"));
 }
}
