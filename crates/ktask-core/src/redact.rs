//! Secret redaction: the shapes a secret comes in, and what one becomes on disk.
//!
//! VISION.md section 11 makes this a requirement rather than a nicety: tokens,
//! credentials and configured patterns are redacted from what a run leaves
//! behind. The outcome it states is stronger than the wording — a secret cannot
//! reach disk — because the two durable files a run writes (the journal and the
//! log) both run their text through here, and neither holds a copy of what this
//! module refused to write.
//!
//! # Shape, not entropy
//!
//! Redaction here is a table of shapes: a provider's key prefix, a bearer
//! token, a URL carrying a password, a name that says "this value is a secret"
//! next to a value. It is deliberately not a high-entropy detector. A scanner
//! that masked anything random-looking would mask the commit shas, base SHAs
//! and gate counters that are the reason the journal is readable, and a run
//! whose evidence has been masked into noise proves nothing. The cost of this
//! choice is a secret in a shape the table does not know; `secret_patterns` in
//! the configuration is the answer to that, and it exists so an operator can
//! name the shape their organisation's keys have without waiting for a release.
//!
//! # The mask is the whole value
//!
//! A match becomes [`MASK`] and nothing of the value survives — not its length,
//! not its last four characters. Redaction that keeps an end or a length keeps
//! part of the secret, and partial values are still recognisable to the person
//! who issued them.
//!
//! Over-redaction is the accepted failure direction. A sentence that loses a
//! phrase reads worse than a sentence that leaks a token reads badly, and the
//! journal is read far more often than a leak is noticed. The tests name both
//! sides: each shape that must go, and the ordinary prose and shas that must
//! stay.
//!
//! # Why a configured pattern is refused rather than skipped
//!
//! [`redact`] answers with a `String`, so it has no channel to report that one
//! of the patterns it was handed is not a regular expression. Skipping a bad
//! pattern silently would be a leak that looks like coverage, so the refusal
//! lives at [`check_patterns`], which every caller that *accepts* patterns from
//! configuration calls before storing them — [`crate::Journal`] refuses a
//! pattern set it cannot honour, at the point it is handed them. A pattern that
//! never got past that door is never handed to [`redact`] at all.
//!
//! # Structure survives redaction
//!
//! [`redact_json`] redacts the *contents of the string literals* in a JSON
//! document — the journal payload and the one-record-per-line log both are one.
//! The document's structure is never in a regex's reach, so a pattern cannot
//! eat a quote and leave a record no reader can parse: the journal's own rule is
//! that a payload it cannot read back is worse than a record it refused to
//! write, and the tests in `journal.rs` hold that rule to it.
//!
//! # Compilation
//!
//! The built-in table is compiled once, on first use, behind a [`std::sync::OnceLock`].
//! Configured patterns are compiled per call, which is the honest cost of a
//! stateless function: they are few, they change only when an operator edits a
//! config file, and a cache keyed on them would be one more thing that can be
//! wrong. A pattern that does not compile is skipped, and
//! [`check_patterns`] is what stops that from ever being the behaviour a run
//! depends on.

use regex::Regex;
use std::sync::OnceLock;

use crate::{Error, Result};

/// What every redacted value becomes.
pub const MASK: &str = "[redacted]";

/// The configuration key a refused pattern came from.
///
/// It is repeated here rather than borrowed from `config` because the refusal
/// has to name the key an operator has to go and edit, and this module does not
/// otherwise know how a configuration was assembled.
const PATTERNS_KEY: &str = "secret_patterns";

/// One built-in secret shape: the pattern that finds it, and what a match of it
/// becomes.
///
/// What the shape *is* is written as a comment above each entry in [`RULES`],
/// because it is there to be read and never to be printed: a rule's name is not
/// a fact about a record, and printing it would put a description of the table
/// into the log the table is there to scrub.
struct Rule {
    pattern: &'static str,
    replacement: &'static str,
}

/// A [`Rule`] whose pattern has been compiled.
struct Compiled {
    pattern: Regex,
    replacement: &'static str,
}

/// The built-in table, in the order it is applied.
///
/// The order is part of the design, not an accident of layout. The key-block
/// rule runs before the header-only rule so a complete block becomes one mask
/// rather than a mask plus a body; the rules that keep the label a credential
/// was copied with run before the bare assignment rule, so an operator reading
/// the line can still tell *which* credential was there.
const RULES: [Rule; 9] = [
    Rule {
        // A private key, header to footer.
        pattern: concat!(
            r"(?s)-----BEGIN[A-Z0-9 ]*PRIVATE KEY-----",
            r".*?",
            r"-----END[A-Z0-9 ]*PRIVATE KEY-----",
        ),
        replacement: MASK,
    },
    Rule {
        // A private key header whose body was cut off mid-block.
        pattern: concat!(r"-----BEGIN[A-Z0-9 ]*PRIVATE KEY-----", r"[^\n]*"),
        replacement: MASK,
    },
    Rule {
        // A provider's key prefix, which is what says the value after it is a key.
        pattern: concat!(
            r"(?x)\b(?:",
            r"[sr]k[-_][A-Za-z0-9_-]{16,}",
            r"|pk[-_][A-Za-z0-9_-]{16,}",
            r"|github_pat_[A-Za-z0-9_]{20,}",
            r"|gh[pousr]_[A-Za-z0-9]{20,}",
            r"|xox[baprse]-[A-Za-z0-9-]{10,}",
            r"|glpat-[A-Za-z0-9_-]{16,}",
            r"|dckr_pat_[A-Za-z0-9_-]{16,}",
            r"|npm_[A-Za-z0-9]{30,}",
            r"|hf_[A-Za-z0-9]{30,}",
            r"|pypi-AgEIcHlwaS5vcmc[A-Za-z0-9_-]{20,}",
            r"|AIza[0-9A-Za-z_-]{30,}",
            r"|(?:AKIA|ASIA)[A-Z0-9]{16}\b",
            r"|(?:AC|SK)[0-9a-f]{32}\b",
            r"|SG\.[A-Za-z0-9_.-]{16,}\.[A-Za-z0-9_.-]{16,}",
            r"|shpat_[A-Za-z0-9]{24,}",
            r")",
        ),
        replacement: MASK,
    },
    Rule {
        // A signed token: three base64 parts, of which the last is the credential.
        pattern: concat!(
            r"\beyJ[A-Za-z0-9_-]{8,}",
            r"\.[A-Za-z0-9_-]{8,}",
            r"\.[A-Za-z0-9_-]{8,}",
        ),
        replacement: MASK,
    },
    Rule {
        // An authorization header, with the scheme that says which credential leaked.
        //
        // The value cannot *start* with `[`, and a bracketed value is matched
        // whole: both are what makes the mask a fixed point of the table. A
        // value class that could enter a mask would match the `[redacted`
        // prefix of an already-masked value and leave one `]` behind on every
        // pass, and a record read out of the journal is redacted again on its
        // way to a log file. Six is the length of `redacted`, so the mask a rule
        // writes is always one whole bracketed match rather than a prefix of one.
        pattern: concat!(
            r"(?i)(\b(?:proxy[-_])?authorization\b",
            r#"["']?\s*[:=]\s*["']?"#,
            r"(?:bearer|basic|token|digest)\s*)",
            r#"(\[[^\]\n]{6,}\]|[^\s"',;}\]\[][^\s"',;}\]\\]*)"#,
        ),
        replacement: "${1}[redacted]",
    },
    Rule {
        // A bare scheme and value, with no header name to hold it.
        pattern: concat!(
            r"(?i)(\b(?:bearer|basic|digest|token)\s+)",
            r"([A-Za-z0-9._~+/=-]{20,})",
        ),
        replacement: "${1}[redacted]",
    },
    Rule {
        // The password half of a URL credential, leaving the address readable.
        pattern: concat!(
            r#"(?i)([a-z][a-z0-9+.-]*://[^\s/@:"`]*:)"#,
            r#"[^\s@/"`]*@"#,
        ),
        replacement: "${1}[redacted]@",
    },
    Rule {
        // A credential used as the username of a URL, where there is no password.
        pattern: concat!(r"(?i)([a-z][a-z0-9+.-]*://)", r"[^\s/@:]+@"),
        replacement: "${1}[redacted]@",
    },
    Rule {
        // A name that says *this value is a secret*, next to a value.
        //
        // Its value alternatives obey the same fixed-point rule as the header
        // rule above: a bracketed value is matched whole, and a bare value
        // cannot start with the mask's `[`.
        pattern: concat!(
            r"(?ix)(",
            r"[A-Za-z0-9_.-]*",
            r"(?:access[_-]?key[_-]?id|secret[_-]?access[_-]?key|client[_-]?secret",
            r"|refresh[_-]?token|access[_-]?token|id[_-]?token|api[_-]?key|apikey",
            r"|private[_-]?key|signing[_-]?key|authorization|password|passwd",
            r"|credential|secret|token|auth)",
            r"[A-Za-z0-9_.-]*)",
            r#"(["']?\s*[:=]\s*["']?)"#,
            concat!(
                r#"(?: " [^"\n]{8,} " |"#,
                r#" ' [^'\n]{8,} ' | \[[^\]\n]{6,}\] |"#,
                r#" [^\s"',;}\]\[][^\s"',;}\]\\]{7,} )"#,
            ),
        ),
        replacement: "${1}${2}[redacted]",
    },
];

/// The built-in table, compiled once for the life of the process.
///
/// A pattern that does not compile is skipped rather than panicking — and
/// `built_in().len() == RULES.len()` is asserted in the tests, so a skipped
/// built-in is a failing build rather than a hole only a leak would show.
fn built_in() -> &'static [Compiled] {
    static COMPILED: OnceLock<Vec<Compiled>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        RULES
            .iter()
            .filter_map(|rule| {
                Regex::new(rule.pattern).ok().map(|pattern| Compiled {
                    pattern,
                    replacement: rule.replacement,
                })
            })
            .collect()
    })
}

/// The patterns the configuration added, compiled.
///
/// A pattern that does not compile is left out, because [`redact`] has no error
/// to return with it — [`check_patterns`] is what keeps that from ever being the
/// behaviour a run depends on.
fn configured(patterns: &[String]) -> Vec<Regex> {
    patterns
        .iter()
        .filter_map(|pattern| Regex::new(pattern).ok())
        .collect()
}

/// Run the built-in table over `text`, then every compiled `extra`.
///
/// The order is the one [`redact`] documents.
///
/// Redaction is a fixed point over its own output: [`MASK`] is the whole of what
/// a rule writes, and no built-in rule can start matching inside a mask it wrote
/// earlier — the two rules that keep a label refuse a value whose first character
/// is the mask's `[`, and match a bracketed value whole instead. That matters
/// because the same text is redacted more than once: agent output is redacted on
/// its way into the journal and again on its way to a log line. A rule that could
/// enter a mask would add one `]` per pass, and the second guarantee an operator
/// reads the journal for — that it says what happened — would rot one character at
/// a time. A *configured* pattern is the operator's own and is applied as given:
/// one matching a prefix of the mask moves the mask, which is why the shapes in
/// `secret_patterns` should name secrets rather than tokens.
fn apply(text: &str, extra: &[Regex]) -> String {
    let mut redacted = text.to_owned();
    for rule in built_in() {
        redacted = rule
            .pattern
            .replace_all(&redacted, rule.replacement)
            .into_owned();
    }
    for pattern in extra {
        redacted = pattern.replace_all(&redacted, MASK).into_owned();
    }
    redacted
}

/// The byte offset of the quote that closes the string literal `from_quote`
/// starts with, or [`None`] when no quote ever does.
///
/// A backslash skips the character it escapes, which is the difference between
/// finding the end of `"a\"b"` and stopping at the quote inside it.
fn literal_end(from_quote: &str) -> Option<usize> {
    let mut offset = 1;
    loop {
        let rest = from_quote.get(offset..)?;
        let found = rest.find(['"', '\\'])?;
        if rest.as_bytes().get(found) == Some(&b'"') {
            return Some(offset + found);
        }
        offset += found + 2;
    }
}

/// Replace every secret-shaped value in `text` with [`MASK`].
///
/// `extra` holds the patterns the configuration added to the built-in table
/// (`secret_patterns`), and each of them is applied in full: a value that
/// matches two patterns is still one mask.
///
/// Patterns are applied to the text as it stands, so this is what a log line or
/// a message goes through; [`redact_json`] is what a structured record goes
/// through.
///
/// A pattern in `extra` that is not a regular expression is skipped. That is the
/// one thing this function cannot report, and [`check_patterns`] exists so that
/// no caller has to depend on it.
#[must_use]
pub fn redact(text: &str, extra: &[String]) -> String {
    apply(text, &configured(extra))
}

/// Redact every string literal inside a JSON document, leaving its structure
/// exactly as it was.
///
/// Each literal's contents are decoded, passed through [`redact`], and
/// re-encoded only when redaction changed them — so a record that holds no
/// secret comes back byte for byte as it went in, and one that does holds the
/// mask where the value was and nothing else different about it.
///
/// # Errors
///
/// [`Error::Serde`] when `text` is not a JSON document, which is refused rather
/// than redacted: text that is not JSON has no literals to scope the patterns
/// to, and treating the whole document as one would let a pattern rewrite the
/// structure it is supposed to leave alone.
///
/// The answer is checked before it is handed back — the redacted document is
/// parsed once more — so no caller below this one can be handed a record that is
/// no longer a record. One parse is a cheap price for the guarantee
/// `journal.rs`'s own tests hold this to: a payload that cannot be read back
/// never reaches the file.
pub fn redact_json(text: &str, extra: &[String]) -> Result<String> {
    let extras = configured(extra);
    let mut record = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(rest) = text.get(cursor..) {
        let Some(offset) = rest.find('"') else {
            record.push_str(rest);
            break;
        };
        let (structure, from_quote) = rest.split_at(offset);
        record.push_str(structure);
        let Some(close) = literal_end(from_quote) else {
            // A literal nothing closes cannot be decoded, so it is redacted as
            // plain text and the parse below is what refuses the record. Nothing
            // is copied through on the assumption that it held no secret.
            record.push_str(&apply(from_quote, &extras));
            break;
        };
        let (quoted, _) = from_quote.split_at(close + 1);
        let decoded: String = serde_json::from_str(quoted)?;
        let redacted = apply(&decoded, &extras);
        if redacted == decoded {
            record.push_str(quoted);
        } else {
            record.push_str(&serde_json::to_string(&redacted)?);
        }
        cursor += offset + close + 1;
    }
    serde_json::from_str::<serde::de::IgnoredAny>(&record)?;
    Ok(record)
}

/// Refuse a configured pattern set that cannot be honoured.
///
/// This is the door configuration patterns come through: it answers before
/// anything is stored, so an operator who typos a pattern finds out at the
/// command that carried it instead of at the moment a secret reached disk
/// unredacted.
///
/// # Errors
///
/// [`Error::Config`] for the first pattern that is not a regular expression,
/// naming the pattern and what `regex` made of it.
///
/// [`Error::Config`]: crate::Error::Config
pub fn check_patterns(patterns: &[String]) -> Result<()> {
    for pattern in patterns {
        if pattern.trim().is_empty() {
            return Err(Error::Config {
                key: PATTERNS_KEY.to_owned(),
                detail: "an empty pattern matches everywhere, so redacting with it would \
                        replace every record rather than the secrets inside it"
                    .to_owned(),
            });
        }
        if let Err(reason) = Regex::new(pattern) {
            return Err(Error::Config {
                key: PATTERNS_KEY.to_owned(),
                detail: format!(
                    "pattern `{pattern}` is not a regular expression, and skipping it at the \
                     moment a secret is being written would be a leak that looks like \
                     coverage: {reason}"
                ),
            });
        }
    }
    Ok(())
}

/// Planted credentials for this crate's tests.
///
/// A test that proves a secret is gone has to plant one first, and a planted
/// credential must not be written into the repository. Push protection on the
/// remote cannot tell a planted fixture from a credential somebody issued and
/// refuses the commit either way, which is the recognition this module sells
/// pointed back at its own test suite. Splitting a value into two adjacent
/// literals is not enough: the halves sit next to each other in the file, and a
/// scanner that rejoins neighbouring literals sees a credential anyway. What
/// these helpers buy is the stronger property, and it is the one VISION.md
/// section 11 asks of what a run leaves behind — no credential, whole or in
/// pieces, is in the bytes of this repository or of its history.
///
/// A body is generated from a seed, so every run plants the same value and a
/// test can assert on it. Its length and alphabet are the issuer's real ones,
/// because a pattern that only matches a short placeholder matches nothing in
/// the wild; a generated body that stopped matching the rule under test would
/// fail `every_built_in_secret_shape_is_redacted_to_the_mask` rather than pass
/// quietly, which is what makes generation safe to use here.
#[cfg(test)]
pub(crate) mod fixtures {
    /// The character set an issuer's credential body is drawn from.
    #[derive(Clone, Copy)]
    pub(crate) enum Alphabet {
        /// Letters and digits, which is most of the table.
        Alnum,
        /// Uppercase letters and digits, which is a cloud access key id.
        Upper,
        /// Lowercase hex, which is a messaging api secret.
        LowerHex,
        /// Url-safe base64, which is the signature half of a signed token.
        Base64Url,
    }

    const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const UPPER: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    const LOWER_HEX: &[u8] = b"0123456789abcdef";
    const BASE64_URL: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";

    impl Alphabet {
        /// The characters of the alphabet.
        pub(crate) const fn chars(self) -> &'static [u8] {
            match self {
                Self::Alnum => ALNUM,
                Self::Upper => UPPER,
                Self::LowerHex => LOWER_HEX,
                Self::Base64Url => BASE64_URL,
            }
        }
    }

    /// A credential body of `len` characters from `alphabet`, drawn from `seed`
    /// so that the same call always plants the same value. The arithmetic is
    /// `usize` throughout, so the body of a fixture is the same width as the
    /// platform's pointer — which is the only width the tests are run at.
    pub(crate) fn body(alphabet: Alphabet, len: usize, seed: usize) -> String {
        let chars = alphabet.chars();
        let mut state = seed.wrapping_add(0x2545_F491_4F6C_DD1D);
        let mut value = String::with_capacity(len);
        for _ in 0..len {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let pick = (state >> 32) % chars.len();
            value.push(char::from(chars[pick]));
        }
        value
    }

    /// A whole credential: the issuer's real prefix and a generated body of the
    /// real length and alphabet.
    pub(crate) fn credential(prefix: &str, alphabet: Alphabet, len: usize, seed: usize) -> String {
        format!("{prefix}{}", body(alphabet, len, seed))
    }

    /// The seed of the one shape more than one module plants, kept here so both
    /// plants the same value.
    pub(crate) const SEED_GITHUB: usize = 4;

    /// A source-control token: the credential a failed `git push` prints, which
    /// is the shape this project is most exposed to. The journal and the
    /// redaction table both plant it, so a leak cannot hide in a value only one
    /// test has ever seen.
    pub(crate) fn github_token() -> String {
        credential("ghp_", Alphabet::Alnum, 36, SEED_GITHUB)
    }

    /// A registry token carried as the username half of a url, which is a shape
    /// the password rules never see.
    pub(crate) fn npm_token() -> String {
        credential("npm_", Alphabet::Alnum, 40, 10)
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{Alphabet, body, credential, github_token, npm_token};
    use super::{MASK, RULES, built_in, check_patterns, redact, redact_json};
    use crate::Error;
    use serde_json::json;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    /// No pattern is in `extra` for most of these: the built-in table is what is
    /// under test, and an empty `extra` is what a run with no configured
    /// patterns hands to every redaction call.
    const NO_EXTRA: &[String] = &[];

    // Seeds are per shape: a fixture that changes is a change to that shape and
    // not to every other one. Three shapes are used by more than one test, so
    // they are named; the rest are named by their place in the table.
    const SEED_OPENAI: usize = 1;
    const SEED_SENDGRID_FIRST: usize = 22;
    const SEED_SENDGRID_SECOND: usize = 23;
    const SEED_JWT_SIGNATURE: usize = 21;

    /// The header and claims of a signed token are not the credential: they are
    /// base64 of `{"alg":"HS256","typ":"JWT"}` and whoever the token names, and
    /// a reader of a redacted line needs them to know a signed token was there.
    const JWT_HEADER: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9";
    const JWT_CLAIMS: &str = "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4iLCJpYXQiOjE3NTAwMDAwMDB9";

    /// The signature half of a signed token, which is the half that is the
    /// credential and so is generated like every other body here.
    fn jwt_signature() -> String {
        body(Alphabet::Base64Url, 43, SEED_JWT_SIGNATURE)
    }

    /// The two-part key of a mail provider, separated by a dot the rule has to
    /// see. Neither half survives redaction, so neither is written.
    fn sendgrid_key() -> String {
        format!(
            "SG.{}.{}",
            body(Alphabet::Alnum, 20, SEED_SENDGRID_FIRST),
            body(Alphabet::Alnum, 42, SEED_SENDGRID_SECOND)
        )
    }

    /// Every shape a secret arrives in: what it is called, the sentence a run
    /// logged up to the credential, and the credential — the issuer's real
    /// prefix with a body of the real length and alphabet.
    ///
    /// The bodies come from the `fixtures` module rather than from the file, for
    /// the reason stated there; the prefixes stay because a prefix is what the
    /// rule matches on and is not a credential on its own. A body that stopped
    /// matching its rule would fail the test below rather than pass quietly.
    fn secret_shapes() -> Vec<(&'static str, String, String)> {
        [
            (
                "an OpenAI key",
                "OPENAI was called with",
                credential("sk-", Alphabet::Alnum, 32, SEED_OPENAI),
            ),
            (
                "an Anthropic key",
                "the anthropic adapter sent",
                credential("sk-ant-api03-", Alphabet::Alnum, 31, 2),
            ),
            (
                "a Stripe live secret key",
                "stripe charged with",
                credential("sk_live_", Alphabet::Alnum, 25, 3),
            ),
            ("a GitHub classic token", "pushed as", github_token()),
            (
                "a GitHub fine-grained token",
                "the fine grained token",
                credential("github_pat_", Alphabet::Alnum, 76, 5),
            ),
            (
                "a GitHub OAuth token",
                "the app installed with",
                credential("ghs_", Alphabet::Alnum, 40, 6),
            ),
            (
                "a Slack bot token",
                "slack posted from",
                credential("xoxb-", Alphabet::Alnum, 49, 7),
            ),
            (
                "a GitLab project token",
                "the registry login used",
                credential("glpat-", Alphabet::Alnum, 20, 8),
            ),
            (
                "a Docker Hub token",
                "docker login used",
                credential("dckr_pat_", Alphabet::Alnum, 27, 9),
            ),
            ("an npm token", "npm published with", npm_token()),
            (
                "a Hugging Face token",
                "the model pushed with",
                credential("hf_", Alphabet::Alnum, 34, 11),
            ),
            (
                "a PyPI upload token",
                "twine uploaded with",
                credential("pypi-AgEIcHlwaS5vcmc", Alphabet::Base64Url, 34, 12),
            ),
            (
                "a Google API key",
                "the maps call used",
                credential("AIza", Alphabet::Alnum, 35, 13),
            ),
            (
                "an AWS access key id",
                "aws refused",
                credential("AKIA", Alphabet::Upper, 16, 14),
            ),
            (
                "an AWS temporary key id",
                "the assumed role held",
                credential("ASIA", Alphabet::Upper, 16, 15),
            ),
            (
                "a Twilio account sid",
                "twilio said",
                credential("AC", Alphabet::LowerHex, 32, 16),
            ),
            (
                "a Twilio api secret",
                "its api secret was",
                credential("SK", Alphabet::LowerHex, 32, 17),
            ),
            ("a SendGrid key", "sendgrid accepted", sendgrid_key()),
            (
                "a Shopify admin token",
                "shopify answered",
                credential("shpat_", Alphabet::Alnum, 32, 19),
            ),
            (
                "a restricted OpenAI key",
                "the restricted key",
                credential("rk-", Alphabet::Alnum, 24, 20),
            ),
        ]
        .into_iter()
        .map(|(what, logged, secret)| (what, format!("{logged} {secret}"), secret))
        .collect()
    }

    /// Assert the planted value is gone, the mask is there, and the sentence
    /// around the value is still the sentence that was written. Only the second
    /// half of that separates "nothing was redacted" from "the secret was
    /// redacted": both leave the secret absent.
    fn assert_shape_redacted(what: &str, text: &str, secret: &str) {
        let redacted = redact(text, NO_EXTRA);
        assert!(
            !redacted.contains(secret),
            "{what} survived redaction: {redacted}"
        );
        assert!(
            redacted.contains(MASK),
            "{what} was refused, but nothing wrote the mask in its place: {redacted}"
        );
    }

    #[test]
    fn every_built_in_secret_shape_is_redacted_to_the_mask() {
        for (what, text, secret) in secret_shapes() {
            assert_shape_redacted(what, &text, &secret);
        }
    }

    /// The point of keeping the label: an operator reading a redacted line has
    /// to be able to tell which credential leaked, or the line says nothing more
    /// than "something was hidden here".
    #[test]
    fn a_bearer_token_keeps_the_scheme_that_names_it_and_redacts_the_value() {
        for (scheme, token) in [
            ("Bearer", "dQw4w9WgXcQdQw4w9WgXcQdQw4w9WgXcQ"),
            ("Basic", "dXNlcjpwbGFjZWhvbGRlclBhc3NwaHJhc2U3Nw=="),
            ("token", "0123456789abcdef0123456789abcdef"),
        ] {
            let text = format!("sent the header Authorization: {scheme} {token} to the remote");
            let redacted = redact(&text, NO_EXTRA);
            assert!(
                !redacted.contains(token),
                "the {scheme} credential survived: {redacted}"
            );
            assert!(
                redacted.contains(&format!("Authorization: {scheme} {MASK}")),
                "the header and its scheme are kept, so the line still says which \
                 credential was there: {redacted}"
            );
        }
    }

    /// Lower-case spelling and a JSON-shaped header are the two ways an agent
    /// quotes a header back that the canonical spelling would miss.
    #[test]
    fn an_authorization_header_is_redacted_in_any_spelling_it_was_copied_in() {
        for text in [
            "the proxy-authorization: basic dXNlcjphcGFzc3dvcmRwbGFjZWhvbGRlcjEyMzQ1 rejected it",
            r#"curl failed on "Authorization" = "Bearer 8fJdKvLmNpQrStUvWxYz1234a5b6c7d8" "#,
        ] {
            let redacted = redact(text, NO_EXTRA);
            assert!(
                redacted.to_lowercase().contains("authorization")
                    && redacted.contains(MASK)
                    && !redacted.contains("8fJdKvLmNpQrStUvWxYz1234a5b6c7d8")
                    && !redacted.contains("dXNlcjphcGFzc3dvcmRwbGFjZWhvbGRlcjEyMzQ1"),
                "the header name was kept and its value refused: {redacted}"
            );
        }
    }

    #[test]
    fn a_bare_bearer_token_is_redacted_without_any_label_to_hold_it() {
        let text =
            "the retry sent bearer YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXo0NTY3ODkw to the socket";
        let redacted = redact(text, NO_EXTRA);
        assert!(
            !redacted.contains("YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXo0NTY3ODkw"),
            "the bare bearer value survived: {redacted}"
        );
        assert!(
            redacted.starts_with("the retry sent bearer "),
            "the word that says what was hidden stays: {redacted}"
        );
    }

    /// A signed token is three base64 parts, and the signature half is the part
    /// that must not survive: it is the credential.
    #[test]
    fn a_json_web_token_is_redacted_whole() {
        let signature = jwt_signature();
        let token = format!("{JWT_HEADER}.{JWT_CLAIMS}.{signature}");
        let text = format!("the provider answered with a 401 and {token} in the body");
        let redacted = redact(&text, NO_EXTRA);
        assert!(!redacted.contains(&token), "the jwt survived: {redacted}");
        assert!(
            !redacted.contains(&signature),
            "the signature half of the jwt survived on its own: {redacted}"
        );
    }

    /// A remote URL carrying a credential is the leak this project is most
    /// exposed to, because it runs `git push` with one.
    #[test]
    fn a_url_credential_is_redacted_and_the_host_stays_readable() {
        let github = github_token();
        let npm = npm_token();
        for (url, secret) in [
            (
                format!("https://x-access-token:{github}@github.com/IllyaYalovyy/ktask-rs.git"),
                github.clone(),
            ),
            (
                "https://runner:S3cretPassphraseForTheRegistry@nexus.internal.example/repository/ktask"
                    .to_owned(),
                "S3cretPassphraseForTheRegistry".to_owned(),
            ),
            (
                "postgres://ktask_run:mt9KxLpQrStUvWxYz2bHc4dNeFg6h@db.internal.example:5432/supervisor"
                    .to_owned(),
                "mt9KxLpQrStUvWxYz2bHc4dNeFg6h".to_owned(),
            ),
            (
                format!("https://{npm}@registry.npmjs.org/"),
                npm.clone(),
            ),
        ] {
            let redacted = redact(&url, NO_EXTRA);
            assert!(
                !redacted.contains(&secret),
                "the url secret survived: {redacted}"
            );
            assert!(redacted.contains(MASK), "no mask was written: {redacted}");
            assert!(
                redacted.contains('@'),
                "an address that hides its credential has to say it was an authenticated \
                 one: {redacted}"
            );
        }
        let host = redact(
            "https://x-access-token:abcdef@github.com/IllyaYalovyy/ktask-rs.git",
            NO_EXTRA,
        );
        assert!(
            host.contains("github.com/IllyaYalovyy/ktask-rs.git"),
            "which remote was refused is the fact an operator needs: {host}"
        );
    }

    /// `token: none` is not a secret and a port number is not a password. This
    /// is the test that keeps redaction from eating the journal's evidence: the
    /// commit shas and counters a replay is compared against have to come back
    /// as they went in.
    #[test]
    fn ordinary_prose_shas_and_counters_are_left_exactly_as_written() {
        for text in [
            "",
            "the verify gate failed with 1 test failed and 399 passed",
            "base_sha 8f3c1e2d4b5a69788796a5b4c3d2e1f091827364 is the commit every later commit is checked against",
            "published ac1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f90 at attempt 2",
            "https://github.com/IllyaYalovyy/ktask-rs and https://example.com:8443/health",
            "token: none was configured for the dummy provider",
            "max_attempts = 3 and secret_patterns = [] are the defaults",
            "author: the reviewer acknowledged the gate and the password field was renamed",
            "Bearer tokens rotate every ninety days and authentication is deprecated",
            "task 35 queued: Secret redaction, whose outcome is that secrets cannot reach disk",
            "the key point is that the key was never in the payload",
        ] {
            assert_eq!(
                redact(text, NO_EXTRA),
                text,
                "redaction changed ordinary text: {text}"
            );
        }
    }

    #[test]
    fn a_secret_named_assignment_loses_its_value_and_keeps_the_name_that_explains_it() {
        for (text, secret) in [
            (
                "OPENAI_API_KEY=sk-notarealkeybutlongenough1234",
                "sk-notarealkeybutlongenough1234",
            ),
            (
                "export ANTHROPIC_AUTH_TOKEN='Wx9Yz1a2b3c4d5e6f7g8h' # rotated",
                "Wx9Yz1a2b3c4d5e6f7g8h",
            ),
            (
                r#"{"api_key": "8fJdKvLmNpQrStUvWxYz1234", "model": "dummy"}"#,
                "8fJdKvLmNpQrStUvWxYz1234",
            ),
            (
                "the gate printed password: hunter2hunter2 for the fixture",
                "hunter2hunter2",
            ),
            (
                "CI: GITLAB_ACCESS_TOKEN = glpat-nottherealthing01",
                "glpat-nottherealthing01",
            ),
            (
                "the gate printed password: [hunter2hunter2] for the fixture",
                "hunter2hunter2",
            ),
        ] {
            let redacted = redact(text, NO_EXTRA);
            assert!(
                !redacted.contains(secret),
                "the assigned secret survived: {redacted}"
            );
            assert!(redacted.contains(MASK), "no mask was written: {redacted}");
        }
        let kept = redact(
            r#"{"base_sha": "8f3c1e2d4b5a69788796a5b4c3d2e1f091827364", "pid": 4211}"#,
            NO_EXTRA,
        );
        assert_eq!(
            kept, r#"{"base_sha": "8f3c1e2d4b5a69788796a5b4c3d2e1f091827364", "pid": 4211}"#,
            "a sha and a pid are the journal's own evidence and are not secrets: {kept}"
        );
    }

    #[test]
    fn a_private_key_is_redacted_from_its_header_to_its_footer() {
        let key = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtz\na2-yWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXp6eXp4eXp4eXo=\n-----END OPENSSH PRIVATE KEY-----";
        let text = format!("the agent printed:\n{key}\nand stopped");
        let redacted = redact(&text, NO_EXTRA);
        assert!(
            !redacted.contains("PRIVATE KEY"),
            "the key block survived: {redacted}"
        );
        assert!(
            !redacted.contains("b3BlbnNzaC1rZXktdjEAAAAA"),
            "the key body survived: {redacted}"
        );
    }

    /// Agent output arrives one line at a time, so a key block is usually seen
    /// as its header line with the body already gone. The header alone still
    /// says a key was there, which is more than an operator needs.
    #[test]
    fn a_private_key_that_was_cut_off_mid_block_redacts_the_line_it_reached() {
        let redacted = redact(
            "-----BEGIN RSA PRIVATE KEY----- MIIEpAIBAAKCAQEA1qX7vYcQwPz2LmNtR4bHgKdJsFvUcXeYz",
            NO_EXTRA,
        );
        assert_eq!(
            redacted, MASK,
            "the header line of a truncated key survived: {redacted}"
        );
    }

    #[test]
    fn a_configured_pattern_reaches_what_the_built_in_table_cannot_see() {
        let extra = ["acme-corp-[a-z0-9]{12}".to_owned()];
        let redacted = redact(
            "the tenant key acme-corp-kx7lmpqr8n2twas rotated at midnight",
            &extra,
        );
        assert!(
            !redacted.contains("acme-corp-kx7lmpqr8n2t"),
            "the configured pattern was not applied: {redacted}"
        );
        assert!(redacted.contains(MASK), "no mask was written: {redacted}");
    }

    /// Each configured pattern is applied, not just the first: a configuration
    /// with three patterns that enforces one of them is a leak with three
    /// patterns in it.
    #[test]
    fn every_configured_pattern_applies_and_not_only_the_first_of_them() {
        let extra = [
            "alpha-[0-9]{6}".to_owned(),
            "beta-[0-9]{6}".to_owned(),
            "gamma-[0-9]{6}".to_owned(),
        ];
        let redacted = redact("alpha-123456 beta-234567 gamma-345678", &extra);
        assert_eq!(
            redacted,
            format!("{MASK} {MASK} {MASK}"),
            "not every configured pattern was applied"
        );
    }

    #[test]
    fn a_pattern_that_is_not_a_regex_is_refused_before_it_could_be_skipped() {
        let refused = check_patterns(&["sk-[A-Za-z0-9]+".to_owned(), "(unclosed".to_owned()])
            .expect_err("a pattern that does not compile is not a pattern");

        let Error::Config { key, detail } = refused else {
            panic!("the refusal names the configuration key that carried the pattern: {refused}");
        };
        assert_eq!(
            key, "secret_patterns",
            "the refusal names the key it came from"
        );
        assert!(
            detail.contains("(unclosed"),
            "the refusal quotes the pattern the operator has to go fix: {detail}"
        );
        check_patterns(&[
            "ghp_[A-Za-z0-9]{36}".to_owned(),
            "sk-[A-Za-z0-9]+".to_owned(),
        ])
        .expect("a sound pattern set is accepted");
        let empty = check_patterns(&[String::new()])
            .expect_err("an empty pattern matches everywhere, so it is not a pattern");
        assert!(
            empty.to_string().contains("everywhere"),
            "the refusal says what an empty pattern would have done: {empty}"
        );
        check_patterns(&[]).expect("the default configuration adds no patterns");
    }

    /// Every built-in has to have compiled, or the table shipped with a hole in
    /// it that only a leak would show. `built_in` skips what it cannot compile,
    /// so the counts are what say none was skipped.
    #[test]
    fn no_built_in_shape_is_skipped_because_every_pattern_in_the_table_compiles() {
        assert_eq!(
            built_in().len(),
            RULES.len(),
            "the compiled table is smaller than the table, which means a built-in pattern \
             failed to compile and its shape is now leaking"
        );
        for rule in RULES {
            assert!(
                rule.replacement.contains(MASK),
                "rule `{}` replaces a match with something that is not the mask, so it hides \
                 nothing",
                rule.pattern
            );
        }
    }

    /// Redaction runs on text that may already have been redacted once: a log
    /// record read out of the journal is redacted again on its way to a file.
    /// A second pass that mangles what the first one wrote makes the evidence
    /// unreadable, and a second pass that grows the text grows without bound.
    #[test]
    fn redaction_is_idempotent_so_a_second_pass_costs_the_record_nothing() {
        for (what, text, secret) in secret_shapes() {
            let once = redact(&text, NO_EXTRA);
            let twice = redact(&once, NO_EXTRA);
            assert_eq!(
                twice, once,
                "{what} was changed a second time by redacting it again"
            );
            assert_eq!(
                redact(&once, &["sk[-_].*".to_owned()]),
                once,
                "{what} grew or shifted when a configured pattern was run over the mask"
            );
            assert!(!twice.contains(&secret));
        }
    }

    /// The rules that keep a label are the ones a second pass could grow: their
    /// replacement is the label plus the mask, so a value class that stops short
    /// of the mask's closing bracket leaves one `]` behind every time the text is
    /// redacted — and a record read out of the journal is redacted again on its
    /// way to a log file, so "again" is not hypothetical. The URL rules get this
    /// for free; these two have to be written to get it.
    #[test]
    fn a_second_pass_grows_nothing_that_kept_its_label() {
        let url_credential = format!(
            "https://x-access-token:{}@github.com/IllyaYalovyy/ktask-rs.git",
            github_token()
        );
        for (what, text) in [
            (
                "an authorization header",
                "sent the header Authorization: Bearer dQw4w9WgXcQdQw4w9WgXcQdQw4w9WgXcQ to the remote",
            ),
            (
                "a quoted authorization header",
                r#"curl failed on "Authorization" = "Bearer 8fJdKvLmNpQrStUvWxYz1234a5b6c7d8" "#,
            ),
            (
                "a bare bearer value",
                "the retry sent bearer YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXo0NTY3ODkw to the socket",
            ),
            (
                "a secret named assignment",
                "OPENAI_API_KEY=sk-notarealkeybutlongenough1234",
            ),
            (
                "a quoted secret named assignment",
                r#"{"api_key": "8fJdKvLmNpQrStUvWxYz1234", "model": "dummy"}"#,
            ),
            ("a url credential", url_credential.as_str()),
            (
                "a deployment secret",
                "deploy with password: hunter2hunter2 to the registry",
            ),
            (
                "a bracketed deployment secret",
                "deploy with password: [hunter2hunter2] to the registry",
            ),
            (
                "a bracketed authorization value",
                "sent the header Authorization: Bearer [dQw4w9WgXcQdQw4w9WgXcQdQw4w9WgXcQ] to the remote",
            ),
        ] {
            let once = redact(text, NO_EXTRA);
            assert!(
                once.contains(MASK),
                "{what} was refused without a mask: {once}"
            );
            assert!(
                !once.contains("]]"),
                "{what} left a bracket behind the mask, the signature of a value class that                  stops short of it: {once}"
            );
            let twice = redact(&once, NO_EXTRA);
            assert_eq!(twice, once, "{what} was changed a second time: {twice}");
        }
    }

    #[test]
    fn a_json_record_is_redacted_inside_its_literals_and_nowhere_else() {
        let openai_key = credential("sk-", Alphabet::Alnum, 32, SEED_OPENAI);
        let record = json!({
            "ts": "2026-09-17T04:09:12.000000000Z",
            "level": "warn",
            "task_id": 35,
            "attempt": 1,
            "phase": "Verify",
            "message": format!("gate verify failed: OPENAI_API_KEY={openai_key}"),
        })
        .to_string();

        let redacted = redact_json(&record, NO_EXTRA).expect("a JSON object is a record");

        assert!(!redacted.contains(&openai_key));
        assert!(redacted.contains(MASK));
        let parsed: serde_json::Value =
            serde_json::from_str(&redacted).expect("the redacted record still parses");
        assert_eq!(
            parsed["task_id"].as_u64(),
            Some(35),
            "the fields around the redaction are still the fields they were: {parsed}"
        );
        assert_eq!(
            parsed["phase"].as_str(),
            Some("Verify"),
            "a literal that held no secret came back as it went in"
        );
    }

    /// A payload whose every byte is evidence — a sha, a counter, a phase name —
    /// comes back unchanged, so redaction cannot reorder or respell what a replay
    /// is later compared against.
    #[test]
    fn a_json_record_holding_no_secret_comes_back_byte_for_byte_as_it_went_in() {
        for record in [
            r#"{"kind":"PreflightPassed","base_sha":"8f3c1e2d4b5a69788796a5b4c3d2e1f091827364"}"#,
            r#"{"kind":"AttemptStarted","attempt":1,"protocol":"direct","pid":4211,"base_sha":"aa11bb22cc33dd44ee55ff66aa11bb22cc33dd44"}"#,
            r#"{"kind":"TaskDone"}"#,
            r#"["one","two","three"]"#,
            r#"{"nested":{"deep":[{"leaf":"an ordinary sentence with a number 41"}]}}"#,
        ] {
            assert_eq!(
                redact_json(record, NO_EXTRA).expect("a JSON document is redacted"),
                record,
                "redaction respelled a record that held nothing to redact"
            );
        }
    }

    /// A configured pattern with no respect for structure is the dangerous one,
    /// because it is written by a human editing a config file at speed. The
    /// record still has to be a record afterwards.
    #[test]
    fn a_configured_pattern_that_matches_everything_still_leaves_a_parsable_record() {
        for pattern in [".*", "[^\"]*", "\\w+", ".*?"] {
            let extra = [pattern.to_owned()];
            let record = r#"{"kind":"AgentOutput","attempt":1,"stream":"stderr","text":"line 41"}"#;
            let redacted =
                redact_json(record, &extra).expect("a record is redacted whatever the pattern");
            serde_json::from_str::<serde_json::Value>(&redacted).unwrap_or_else(|reason| {
                panic!(
                    "pattern `{pattern}` left a record that cannot be parsed: {reason}\n{redacted}"
                )
            });
        }
    }

    #[test]
    fn text_that_is_not_a_json_record_is_refused_rather_than_redacted() {
        for text in ["not json at all", "{\"unterminated\":", ""] {
            let refused = redact_json(text, NO_EXTRA)
                .expect_err("text that is not a record has no literals to scope a pattern to");
            assert!(
                matches!(refused, Error::Serde(_)),
                "the refusal is the parser's own: {refused}"
            );
        }
    }

    /// The shape `T086` writes: one JSON record per line, in a file below the
    /// project's state directory. What is planted here is what a provider prints
    /// with its own credentials in the line, and the assertion is on the bytes
    /// of the file rather than on a return value.
    fn write_log(lines: &[String]) -> (TempDir, std::path::PathBuf) {
        let dir = tempdir().expect("a scratch directory");
        let path = dir.path().join("run.jsonl");
        let contents = format!("{}\n", lines.join("\n"));
        fs::write(&path, contents).expect("the log file is written");
        (dir, path)
    }

    /// The credential the log tests plant: the same fixture the shape table
    /// uses for that shape, so a leak cannot hide in a value only one test has
    /// ever seen.
    fn planted() -> String {
        github_token()
    }

    /// One log record, with a secret in the field an agent's own words land in.
    fn log_record(message: &str) -> String {
        json!({
            "ts": "2026-09-17T04:09:12.000000000Z",
            "level": "warn",
            "task_id": 35,
            "attempt": 1,
            "phase": "Implement",
            "message": message,
        })
        .to_string()
    }

    #[test]
    fn a_redacted_log_record_never_reaches_the_bytes_of_the_file_it_was_written_to() {
        let planted = planted();
        let record = log_record(&format!(
            "the provider sent Authorization: Bearer {planted} and the call was refused"
        ));
        let redacted = redact_json(&record, &["acme-[a-z0-9]+".to_owned()])
            .expect("the record is redacted before it is written");
        let (_scratch, path) = write_log(&[redacted]);

        let bytes = fs::read(&path).expect("the written log is read back");
        assert!(
            !bytes
                .windows(planted.len())
                .any(|window| window == planted.as_bytes()),
            "the planted token is in the file: {}",
            String::from_utf8_lossy(&bytes)
        );
        assert!(
            String::from_utf8_lossy(&bytes).contains(MASK),
            "the file holds no mask either, so nothing was redacted and the assertion above \
             passed for the wrong reason"
        );
        for line in String::from_utf8_lossy(&bytes).lines() {
            serde_json::from_str::<serde_json::Value>(line)
                .expect("every line of the log is still one record");
        }
    }

    /// The control that makes the test above worth running: the same bytes are
    /// scanned, and this time the secret is there. Without it, a scan that could
    /// never see a planted value would report success forever.
    #[test]
    fn the_same_scan_finds_the_planted_secret_in_a_log_written_without_redaction() {
        let planted = planted();
        let record = log_record(&format!(
            "the provider sent Authorization: Bearer {planted} and the call was refused"
        ));
        let (_scratch, path) = write_log(&[record]);

        let bytes = fs::read(&path).expect("the written log is read back");
        assert!(
            bytes
                .windows(planted.len())
                .any(|window| window == planted.as_bytes()),
            "the scan cannot see a planted secret in a written file, which makes every other \
             assertion in this module worthless"
        );
    }
}
