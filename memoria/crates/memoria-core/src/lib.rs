pub mod error;
pub mod interfaces;
pub mod sensitivity;
pub mod types;

pub use error::MemoriaError;
pub use sensitivity::{check_sensitivity, SensitivityResult, SensitivityTier};
pub use types::{Memory, MemoryType, TrustTier};

/// Workaround: MO#24001 — PREPARE/EXECUTE stores `Option<String>::None` as empty
/// string `''` instead of SQL NULL for VARCHAR columns.  Normalize at both write
/// (bind) and read boundaries so the rest of the codebase can treat `None` and
/// `Some("")` identically.
///
/// Write-side: normalize for `.bind()` — returns `None` for both `None` and `Some("")`.
#[inline]
pub fn nullable_str(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().filter(|s| !s.is_empty())
}

/// Read-side: normalize after `row.try_get()` — converts `Some("")` back to `None`.
#[inline]
pub fn nullable_str_from_row(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

/// Truncate a string to at most `max_bytes` bytes, rounding down to a valid UTF-8 char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    let len = s.len().min(max_bytes);
    // Find the nearest valid UTF-8 char boundary at or before `len`
    let mut end = len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
