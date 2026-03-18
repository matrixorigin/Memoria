pub mod error;
pub mod interfaces;
pub mod sensitivity;
pub mod types;

pub use error::MemoriaError;
pub use sensitivity::{check_sensitivity, SensitivityResult, SensitivityTier};
pub use types::{Memory, MemoryType, TrustTier};

/// Truncate a string to at most `max_bytes` bytes, rounding down to a valid UTF-8 char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    &s[..s.floor_char_boundary(s.len().min(max_bytes))]
}
