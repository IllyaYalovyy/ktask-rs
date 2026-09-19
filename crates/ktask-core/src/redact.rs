//! Secret redaction for protecting sensitive data in logs and journals.

use regex::Regex;
use std::sync::OnceLock;

static API_KEY_REGEX: OnceLock<Regex> = OnceLock::new();
static BEARER_TOKEN_REGEX: OnceLock<Regex> = OnceLock::new();
static AWS_KEY_REGEX: OnceLock<Regex> = OnceLock::new();
static GITHUB_TOKEN_REGEX: OnceLock<Regex> = OnceLock::new();

/// Redact sensitive information from text.
///
/// Replaces matches of built-in patterns (API keys, bearer tokens, AWS keys,
/// GitHub tokens) and any custom patterns provided in `extra` with `[redacted]`.
///
/// # Arguments
///
/// * `text` - The text to redact
/// * `extra` - Optional custom regex patterns to match and redact
///
/// # Returns
///
/// The redacted text with all secrets replaced with `[redacted]`.
///
/// # Examples
///
/// ```
/// use ktask_core::redact::redact;
/// let text = "My API key is sk-1234567890abcdef";
/// let redacted = redact(text, &[]);
/// assert!(redacted.contains("[redacted]"));
/// assert!(!redacted.contains("sk-"));
/// ```
#[must_use]
pub fn redact(text: &str, extra: &[String]) -> String {
    let mut result = text.to_string();

    // Apply built-in patterns
    result = apply_pattern(&result, get_api_key_regex());
    result = apply_pattern(&result, get_bearer_token_regex());
    result = apply_pattern(&result, get_aws_key_regex());
    result = apply_pattern(&result, get_github_token_regex());

    // Apply custom patterns
    for pattern_str in extra {
        if let Ok(pattern) = Regex::new(pattern_str) {
            result = pattern.replace_all(&result, "[redacted]").to_string();
        }
    }

    result
}

#[allow(clippy::expect_used)]
fn get_api_key_regex() -> &'static Regex {
    API_KEY_REGEX
        .get_or_init(|| Regex::new(r"sk-[a-zA-Z0-9\-_.]+").expect("API key regex is valid"))
}

#[allow(clippy::expect_used)]
fn get_bearer_token_regex() -> &'static Regex {
    BEARER_TOKEN_REGEX.get_or_init(|| {
        Regex::new(r"Bearer\s+[a-zA-Z0-9\-_.]+").expect("Bearer token regex is valid")
    })
}

#[allow(clippy::expect_used)]
fn get_aws_key_regex() -> &'static Regex {
    AWS_KEY_REGEX.get_or_init(|| Regex::new(r"AKIA[0-9A-Z]{16}").expect("AWS key regex is valid"))
}

#[allow(clippy::expect_used)]
fn get_github_token_regex() -> &'static Regex {
    GITHUB_TOKEN_REGEX.get_or_init(|| {
        Regex::new(r"gh[pousr]_[a-zA-Z0-9_]+").expect("GitHub token regex is valid")
    })
}

fn apply_pattern(text: &str, pattern: &Regex) -> String {
    pattern.replace_all(text, "[redacted]").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_api_key() {
        let text = "My API key is sk-1234567890abcdefghij";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("sk-"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_bearer_token() {
        let text = "Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWI";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("Bearer eyJ"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_aws_key() {
        let text = "Access key: AKIAIOSFODNN7EXAMPLE";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("AKIA"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_github_token() {
        let text = "Token: ghp_0123456789abcdefghijklmnopqrstuv";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("ghp_"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_multiple_secrets() {
        let text = "Key1: sk-abc123def456 and Token: Bearer xyz789";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("sk-"));
        assert!(!redacted.contains("Bearer"));
        assert_eq!(redacted.matches("[redacted]").count(), 2);
    }

    #[test]
    fn redact_custom_pattern() {
        let text = "Secret password: super_secret_pass_12345";
        let patterns = vec!["super_secret_[a-z_]+".to_string()];
        let redacted = redact(text, &patterns);
        assert!(!redacted.contains("super_secret_pass"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn redact_leaves_non_secrets() {
        let text = "This is normal text without secrets";
        let redacted = redact(text, &[]);
        assert_eq!(text, redacted);
    }
}
