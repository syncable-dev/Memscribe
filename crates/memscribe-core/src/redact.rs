//! The redaction pass (whitepaper §8.6, §11).
//!
//! A deterministic pass that strips known secret patterns (API keys, tokens,
//! private-key blocks, `.env`-style assignments) from node text *before* the
//! sink. A `--no-content` mode elides all verbatim text and keeps only
//! structure. Redaction is on by default for known secret patterns.

use crate::node::PreparedNode;
use regex::Regex;

/// Default secret-detecting patterns, as `(label, pattern)` pairs.
#[must_use]
fn default_patterns() -> Vec<(&'static str, &'static str)> {
    vec![
        ("anthropic_key", r"sk-ant-[A-Za-z0-9_-]{16,}"),
        ("openai_key", r"sk-[A-Za-z0-9]{20,}"),
        ("aws_access_key", r"AKIA[0-9A-Z]{16}"),
        ("github_token", r"gh[pousr]_[A-Za-z0-9]{20,}"),
        ("slack_token", r"xox[baprs]-[A-Za-z0-9-]{10,}"),
        ("google_api_key", r"AIza[0-9A-Za-z_-]{35}"),
        ("bearer_token", r"(?i)bearer\s+[A-Za-z0-9._~+/-]{16,}=*"),
        (
            "assignment_secret",
            r#"(?i)\b(?:api[_-]?key|secret|token|password|passwd|access[_-]?key)\b\s*[=:]\s*[^\s'"]{6,}"#,
        ),
        (
            "private_key_block",
            r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----[\s\S]*?-----END (?:RSA |EC |OPENSSH |DSA |PGP )?PRIVATE KEY-----",
        ),
    ]
}

/// The redactor.
#[derive(Debug)]
pub struct Redactor {
    patterns: Vec<(String, Regex)>,
    no_content: bool,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::with_default_patterns(false)
    }
}

impl Redactor {
    /// Build a redactor from the default patterns. `no_content` elides all text.
    ///
    /// # Panics
    /// Never in practice — the default patterns are exercised by tests.
    #[must_use]
    pub fn with_default_patterns(no_content: bool) -> Self {
        let patterns = default_patterns()
            .into_iter()
            .map(|(label, pat)| {
                (
                    label.to_string(),
                    Regex::new(pat).expect("default redaction patterns must compile"),
                )
            })
            .collect();
        Redactor {
            patterns,
            no_content,
        }
    }

    /// Build a redactor from custom `(label, pattern)` pairs.
    ///
    /// # Errors
    /// Returns the regex error if any pattern fails to compile.
    pub fn from_patterns<S: AsRef<str>>(
        pairs: impl IntoIterator<Item = (S, S)>,
        no_content: bool,
    ) -> Result<Self, regex::Error> {
        let mut patterns = Vec::new();
        for (label, pat) in pairs {
            patterns.push((label.as_ref().to_string(), Regex::new(pat.as_ref())?));
        }
        Ok(Redactor {
            patterns,
            no_content,
        })
    }

    /// Whether `--no-content` mode is on.
    #[must_use]
    pub fn is_no_content(&self) -> bool {
        self.no_content
    }

    /// Redact a string. In `no_content` mode, returns a structural placeholder.
    /// Otherwise replaces each secret match with `[REDACTED:<label>]`.
    /// Deterministic: patterns are applied in a fixed order.
    #[must_use]
    pub fn redact_text(&self, s: &str) -> String {
        if self.no_content {
            return "[content elided]".to_string();
        }
        let mut out = s.to_string();
        for (label, re) in &self.patterns {
            out = re
                .replace_all(&out, format!("[REDACTED:{label}]").as_str())
                .into_owned();
        }
        out
    }

    /// Whether the text contains any secret (before redaction).
    #[must_use]
    pub fn contains_secret(&self, s: &str) -> bool {
        self.patterns.iter().any(|(_, re)| re.is_match(s))
    }

    /// Redact a prepared node in place (text, epitome, and diff contents).
    pub fn redact_node(&self, node: &mut PreparedNode) {
        match node {
            PreparedNode::Conversation(c) => {
                c.text = self.redact_text(&c.text);
            }
            PreparedNode::Decision(d) => {
                d.epitome = self.redact_text(&d.epitome);
                for opt in &mut d.considered_options {
                    opt.text = self.redact_text(&opt.text);
                }
            }
            PreparedNode::Episode(e) => {
                if let Some(old) = e.diff.old.take() {
                    e.diff.old = Some(self.redact_text(&old));
                }
                if let Some(new) = e.diff.new.take() {
                    e.diff.new = Some(self.redact_text(&new));
                }
                if let Some(u) = e.diff.unified.take() {
                    e.diff.unified = Some(self.redact_text(&u));
                }
            }
            PreparedNode::Binding(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_known_keys() {
        let r = Redactor::default();
        let out = r.redact_text("export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwx1234");
        assert!(!out.contains("sk-abcdefghijklmnopqrst"));
        assert!(out.contains("[REDACTED:"));
    }

    #[test]
    fn no_content_elides_everything() {
        let r = Redactor::with_default_patterns(true);
        assert_eq!(r.redact_text("anything at all"), "[content elided]");
    }

    #[test]
    fn redaction_is_deterministic() {
        let r = Redactor::default();
        let s = "token: ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        assert_eq!(r.redact_text(s), r.redact_text(s));
    }
}
