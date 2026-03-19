//! Sensitivity filter — tiered PII/credential detection for long-term memory.
//!
//! Three tiers:
//!   HIGH   (passwords, API keys, private keys) → block entire memory
//!   MEDIUM (email, phone, SSN, credit card)    → redact in-place, keep memory
//!   LOW    (usernames)                          → allow through unchanged

use std::borrow::Cow;

#[derive(Debug, PartialEq)]
pub enum SensitivityTier {
    High,
    Medium,
}

#[derive(Debug)]
pub struct SensitivityResult {
    /// True if memory must be discarded (HIGH tier match)
    pub blocked: bool,
    /// Redacted content if MEDIUM tier matched; None if content is safe as-is
    pub redacted_content: Option<String>,
    /// Labels of matched patterns
    pub matched_labels: Vec<&'static str>,
}

struct Pattern {
    label: &'static str,
    tier: SensitivityTier,
    regex: &'static str,
    replacement: &'static str,
}

// Patterns defined as static strings; compiled lazily via once_cell
static PATTERNS: &[Pattern] = &[
    // HIGH — block
    Pattern {
        label: "aws_key",
        tier: SensitivityTier::High,
        regex: r"(?:AKIA|ABIA|ACCA|ASIA)[0-9A-Z]{16}",
        replacement: "",
    },
    Pattern {
        label: "private_key",
        tier: SensitivityTier::High,
        regex: r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----",
        replacement: "",
    },
    Pattern {
        label: "bearer_token",
        tier: SensitivityTier::High,
        regex: r"(?i)Bearer\s+[A-Za-z0-9\-._~+/]+=*",
        replacement: "",
    },
    Pattern {
        label: "password_assign",
        tier: SensitivityTier::High,
        regex: r"(?i)(?:password|passwd|secret)\s*[:=]\s*\S+",
        replacement: "",
    },
    // MEDIUM — redact
    Pattern {
        label: "email",
        tier: SensitivityTier::Medium,
        regex: r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z]{2,}",
        replacement: "[email]",
    },
    Pattern {
        label: "phone",
        tier: SensitivityTier::Medium,
        regex: r"\b\d{3}[-.]?\d{3,4}[-.]?\d{4}\b",
        replacement: "[phone]",
    },
    Pattern {
        label: "ssn",
        tier: SensitivityTier::Medium,
        regex: r"\b\d{3}-\d{2}-\d{4}\b",
        replacement: "[ssn]",
    },
    Pattern {
        label: "credit_card",
        tier: SensitivityTier::Medium,
        regex: r"\b(?:\d[ -]*?){13,19}\b",
        replacement: "[card]",
    },
];

use once_cell::sync::Lazy;
use regex::Regex;

static COMPILED: Lazy<Vec<(&'static Pattern, Regex)>> = Lazy::new(|| {
    PATTERNS
        .iter()
        .map(|p| (p, Regex::new(p.regex).expect("valid regex")))
        .collect()
});

/// Check content for PII/credentials. Returns a `SensitivityResult`.
pub fn check_sensitivity(text: &str) -> SensitivityResult {
    // HIGH tier — any match blocks immediately
    for (p, re) in COMPILED.iter() {
        if p.tier == SensitivityTier::High && re.is_match(text) {
            return SensitivityResult {
                blocked: true,
                redacted_content: None,
                matched_labels: vec![p.label],
            };
        }
    }

    // MEDIUM tier — redact all matches
    let mut redacted = Cow::Borrowed(text);
    let mut hits: Vec<&'static str> = Vec::new();

    for (p, re) in COMPILED.iter() {
        if p.tier == SensitivityTier::Medium {
            let result = re.replace_all(&redacted, p.replacement);
            if result != redacted {
                hits.push(p.label);
                redacted = Cow::Owned(result.into_owned());
            }
        }
    }

    if !hits.is_empty() {
        return SensitivityResult {
            blocked: false,
            redacted_content: Some(redacted.into_owned()),
            matched_labels: hits,
        };
    }

    SensitivityResult {
        blocked: false,
        redacted_content: None,
        matched_labels: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aws_key_blocked() {
        let r = check_sensitivity("my key is AKIAIOSFODNN7EXAMPLE");
        assert!(r.blocked);
        assert_eq!(r.matched_labels, vec!["aws_key"]);
    }

    #[test]
    fn test_private_key_blocked() {
        let r = check_sensitivity("-----BEGIN RSA PRIVATE KEY-----\nMIIE...");
        assert!(r.blocked);
    }

    #[test]
    fn test_password_blocked() {
        let r = check_sensitivity("password=supersecret123");
        assert!(r.blocked);
        assert_eq!(r.matched_labels, vec!["password_assign"]);
    }

    #[test]
    fn test_email_redacted() {
        let r = check_sensitivity("contact me at alice@example.com please");
        assert!(!r.blocked);
        assert_eq!(
            r.redacted_content.as_deref(),
            Some("contact me at [email] please")
        );
        assert!(r.matched_labels.contains(&"email"));
    }

    #[test]
    fn test_phone_redacted() {
        let r = check_sensitivity("call 555-867-5309 anytime");
        assert!(!r.blocked);
        assert!(r.redacted_content.as_deref().unwrap().contains("[phone]"));
    }

    #[test]
    fn test_ssn_redacted() {
        let r = check_sensitivity("SSN is 123-45-6789");
        assert!(!r.blocked);
        assert!(r.redacted_content.as_deref().unwrap().contains("[ssn]"));
    }

    #[test]
    fn test_clean_content_passes() {
        let r = check_sensitivity("I prefer Rust over Python for systems programming");
        assert!(!r.blocked);
        assert!(r.redacted_content.is_none());
        assert!(r.matched_labels.is_empty());
    }

    #[test]
    fn test_multiple_medium_redacted() {
        let r = check_sensitivity("email alice@example.com phone 555-123-4567");
        assert!(!r.blocked);
        let redacted = r.redacted_content.unwrap();
        assert!(redacted.contains("[email]"));
        assert!(redacted.contains("[phone]"));
    }
}
