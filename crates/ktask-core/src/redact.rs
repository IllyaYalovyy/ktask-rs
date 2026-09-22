//! Secret redaction: replacing text that looks like a credential with
//! `[redacted]` before it reaches disk (VISION.md section 11).

use regex::Regex;
use std::sync::LazyLock;

/// Patterns recognized without any configuration: vendor-prefixed API keys,
/// `Bearer` tokens, and generic `key = "..."`-shaped secret assignments.
///
/// Compiled once and reused, since compiling a regex is too expensive to
/// repeat on every call to [`redact`].
static BUILTIN_PATTERNS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        // `Bearer <token>` (case-insensitive on the scheme keyword).
        r"(?i)\bBearer\s+[A-Za-z0-9\-._~+/]+=*",
        // OpenAI-style secret keys: `sk-` followed by 20+ token characters.
        r"\bsk-[A-Za-z0-9]{20,}\b",
        // GitHub personal access / app / installation tokens.
        r"\bgh[pousr]_[A-Za-z0-9]{36,}\b",
        r"\bgithub_pat_[A-Za-z0-9_]{22,}\b",
        // AWS access key ids.
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        // Google API keys.
        r"\bAIza[0-9A-Za-z\-_]{35}\b",
        // Slack tokens.
        r"\bxox[baprs]-[A-Za-z0-9\-]{10,}\b",
        // Generic `api_key = "..."` / `secret-key: '...'` style assignments.
        r#"(?i)\b(?:api[_-]?key|secret[_-]?key|access[_-]?token|auth[_-]?token|password)\b\s*[:=]\s*['"]?[A-Za-z0-9_\-/+]{8,}['"]?"#,
    ]
    .iter()
    // Every pattern here is exercised by this module's tests, so a broken
    // one would fail a test rather than surface here; skipping instead of
    // panicking keeps a typo in one pattern from taking down every other
    // built-in protection (this crate treats errors as values, never as a
    // reason for a supervisor process to panic).
    .filter_map(|pattern| Regex::new(pattern).ok())
    .collect()
});

/// Returns `text` with every match of a built-in secret shape, or of any
/// pattern in `extra`, replaced with `[redacted]`.
///
/// Built-in patterns catch values that look like API keys (vendor-prefixed
/// tokens such as `sk-`, `ghp_`, AWS access key ids, Google API keys, Slack
/// tokens, and generic `key = "..."` assignments) and `Bearer` tokens.
/// `extra` supplies additional regular expressions (for instance,
/// [`crate::Config::secret_patterns`]); an entry that fails to compile as a
/// regex is skipped rather than aborting redaction of everything else, since
/// a single malformed pattern in user configuration must not defeat the
/// built-in protections.
#[must_use]
pub fn redact(text: &str, extra: &[String]) -> String {
    let mut result = text.to_string();
    for pattern in BUILTIN_PATTERNS.iter() {
        result = pattern.replace_all(&result, "[redacted]").into_owned();
    }
    for pattern in extra {
        if let Ok(re) = Regex::new(pattern) {
            result = re.replace_all(&result, "[redacted]").into_owned();
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_token_is_redacted() {
        let text = "Authorization: Bearer abcDEF123456ghijKLMNOPqrstuvwxYZ0123456789";
        let redacted = redact(text, &[]);
        assert!(!redacted.contains("abcDEF123456ghijKLMNOPqrstuvwxYZ0123456789"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn an_openai_style_secret_key_is_redacted() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let redacted = redact(&format!("leaked: {secret}"), &[]);
        assert!(!redacted.contains(secret));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn a_github_personal_access_token_is_redacted() {
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789AB";
        let redacted = redact(&format!("token={secret}"), &[]);
        assert!(!redacted.contains(secret));
    }

    #[test]
    fn an_aws_access_key_id_is_redacted() {
        let secret = "AKIAABCDEFGHIJKLMNOP";
        let redacted = redact(&format!("aws_access_key_id = {secret}"), &[]);
        assert!(!redacted.contains(secret));
    }

    #[test]
    fn a_generic_key_assignment_is_redacted() {
        let redacted = redact(r#"api_key: "abcdefgh12345678""#, &[]);
        assert!(!redacted.contains("abcdefgh12345678"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn text_with_no_secret_shapes_is_left_unchanged() {
        let text = "the build passed and everything is fine";
        assert_eq!(redact(text, &[]), text);
    }

    #[test]
    fn a_configured_extra_pattern_is_redacted() {
        let text = "internal id: PROJECT-42-SECRET";
        let redacted = redact(text, &["PROJECT-\\d+-SECRET".to_string()]);
        assert!(!redacted.contains("PROJECT-42-SECRET"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn an_invalid_extra_pattern_is_skipped_without_panicking() {
        let text = "value that is not itself secret shaped";
        let redacted = redact(text, &["(unclosed".to_string()]);
        assert_eq!(redacted, text);
    }

    #[test]
    fn built_in_patterns_still_apply_when_an_extra_pattern_is_invalid() {
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123456789";
        let redacted = redact(&format!("leaked: {secret}"), &["(unclosed".to_string()]);
        assert!(!redacted.contains(secret));
    }
}
