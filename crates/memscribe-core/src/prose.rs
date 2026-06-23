//! Turn-source hygiene: project a user turn down to genuine human prose.
//!
//! The commitment gate is a lexical matcher over user turns. But on real
//! transcripts a "user turn" is frequently *not* human prose: people paste prior
//! agent transcripts (tool-call plumbing, `<tool-use-id>` tokens), tools and
//! skills inject system text into the conversation (`USE MEMTRACE TOOLS FIRST`,
//! gate banners, `<system-reminder>`), and code/log output gets quoted inline.
//! When that content is gated as if it were a human decision, the resulting
//! Decision nodes are junk — the dominant precision failure the real-data audit
//! found (turn-source hygiene, *not* gate vocabulary).
//!
//! [`ProseFilter`] is a **pure, deterministic** projection: given a raw turn it
//! returns the subset of lines that are plausibly human-authored prose, dropping
//! injected / tool / log / code lines, bare id/url/path tokens, and turns with
//! too little substance. The gate then runs over the projection, so neither the
//! Conversation it elevates nor the Decision it seeds is contaminated by
//! plumbing. The verbatim turn is untouched at the event layer — only what gets
//! *elevated to a node* is cleaned, so the lossless contract is preserved.

use regex::Regex;

/// A deterministic human-prose projector for user turns.
#[derive(Debug)]
pub struct ProseFilter {
    /// Line-level reject patterns (injected/tool/system/log/code signatures),
    /// compiled case-insensitively.
    line_reject: Vec<Regex>,
    /// Minimum count of alphabetic word tokens for the projection to count as
    /// substantive prose worth gating.
    min_alpha_words: usize,
}

impl Default for ProseFilter {
    fn default() -> Self {
        Self::default_filter()
    }
}

/// The default line-reject patterns: verbatim signatures of content that is not
/// human prose. Kept inspectable and unit-tested, like the gate's rule table.
#[must_use]
pub fn default_line_rejects() -> Vec<&'static str> {
    vec![
        // ---- tool / harness plumbing (pasted transcript renderings) ----
        r"<tool-use-id>",
        r"</tool-use-id>",
        r"\[Request interrupted",
        r"tool_use_id",
        r"^\s*\*\*Tool Call",
        r"^\s*Status:\s*(?:Completed|Failed|Running)\b",
        r"</?summary>",
        r"</?task-notification>",
        r"</?system-notification>",
        r"\[SYSTEM NOTIFICATION",
        r"automated background-task event",
        r"Dynamic workflow .* completed",
        // ---- injected system / skill / command blocks ----
        r"<system-reminder",
        r"</system-reminder>",
        r"<skill_content",
        r"</skill_content>",
        r"<source>\s*global\s*</source>",
        r"^\s*<directory>",
        r"</?command-[a-z-]+>",
        r"<local-command-stdout>",
        r"session-scoped Stop hook",
        r"Stop hook is now active",
        r"Relative paths in this skill resolve",
        // ---- self-ingestion deny-list (verbatim agent/skill boilerplate) ----
        r"USE MEMTRACE TOOLS FIRST",
        r"IF THE REPO IS INDEXED IN MEMTRACE",
        r"MEMSCRIBE GATE\s*[—-]\s*GROUNDING",
        r"^#+\s*Memtrace First\b",
        r"\bThe Iron Law\b",
        r"Always use first for indexed source-code repos",
        // ---- logs / console output ----
        r"\[(?:info|warn|warning|error|debug|trace)\]",
        r"\blevel=(?:info|warn|warning|error|debug|trace)\b",
        r"^\s*\d+Z\s",                  // "676Z [memtrace] ..."
        r"\b\d{1,2}:\d{2}:\d{2}\b.*\[", // timestamped bracketed log line
        r"^\s*(?:warning|error|note|help):\s", // compiler/diagnostic line
        r"error\[E\d+\]",               // rustc diagnostic code
        // ---- code / diff payloads ----
        r"^\s*\d+\s*\|",                // line-number / diff gutter "74 | ..."
        r"^\s*[+\-]\s*\d+\s*\|",        // diff hunk with gutter
        r"^\s*(?:use|import|from|pub|fn|impl|struct|enum|const|let|class|def|public|private|export|function|return|package|namespace)\b.*(?:;|\{|::|=>)",
        r"^\s*(?://|/\*|\*\s|#include|<\?|```)",
        r"^\s*\|",         // diagnostic/table gutter or a markdown/rustc table row
        r"\|\s+\^",        // rustc underline "  |    ^^^ this value ..."
        r"^\s*\[\s*\x22",  // a JSON string-array open: ["...
        r"\w+\|\w+\|\w+",  // pipe-delimited token table / regex alternation dump
        // ---- pasted agent-analysis prose describing code ----
        r"^\s*[a-z]{1,6}:\d",                 // a file-extension:line ref lead ("rs:1342", "ts:52", "yml:181")
        r"\b\w+\.(?:rs|ts|tsx|js|jsx|py|go|rb|kt|swift|toml|yml|yaml):\d", // path.ext:line
        r"^\s*#{1,6}\s+.*#\d{2,}",            // a markdown header naming an issue ("### #483 — …")
        r"^\s*-?\s*###?\s+#\d",               // an issue-ref heading line
        // ---- pasted agent-output markdown / doc-snippet leftovers ----
        r"^\s*#{2,6}\s",                      // a markdown section header ("## Summary of fixes")
        r"^\s*\*\*[^*]{1,48}:\*\*",           // a bold label header ("**Flag an existing user:**")
        r"^\s*title:\s",                      // frontmatter / commit-title leftover ("title: Quick rebuild")
        r"^\s*\(use\b",                       // a parenthetical hint ("(use \"git add <file>\" …)")
        r"[?!]{4,}",                          // punctuation venting ("…FAILING ?????!!!!!!")
        r"\b(?:SET|WHERE)\s+[\w.]+\s*=",      // a SQL fragment ("… SET unlimited_queries = true …")
        r"\bINSERT\s+INTO\b",                 // a SQL fragment
        r"^\s*\x22,",                         // a JSON string-array leftover (`","essentially …`)
    ]
}

impl ProseFilter {
    /// Build the default filter.
    ///
    /// # Panics
    /// Never in practice — the default patterns are compile-time constants and
    /// are exercised by tests; a malformed default is a build-breaking bug.
    #[must_use]
    pub fn default_filter() -> Self {
        Self::from_line_patterns(default_line_rejects(), 3)
            .expect("default prose-filter patterns must compile")
    }

    /// Build a filter from line-reject patterns and a minimum substance bar.
    ///
    /// # Errors
    /// Returns the underlying regex error if any pattern fails to compile.
    pub fn from_line_patterns<S: AsRef<str>>(
        patterns: impl IntoIterator<Item = S>,
        min_alpha_words: usize,
    ) -> Result<Self, regex::Error> {
        let mut line_reject = Vec::new();
        for p in patterns {
            line_reject.push(Regex::new(&format!("(?i){}", p.as_ref()))?);
        }
        Ok(ProseFilter {
            line_reject,
            min_alpha_words,
        })
    }

    /// The human-prose projection of `text`: junk lines removed, in order.
    /// Returns `None` when nothing substantive remains — the turn should not be
    /// gated at all. **Pure**: depends only on `text`.
    #[must_use]
    pub fn clean(&self, text: &str) -> Option<String> {
        let mut kept: Vec<&str> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if self.is_junk_line(line) {
                continue;
            }
            kept.push(line);
        }
        if kept.is_empty() {
            return None;
        }
        let joined = kept.join("\n");
        if alpha_word_count(&joined) < self.min_alpha_words {
            return None;
        }
        Some(joined)
    }

    /// Whether `text` contains any gateable human prose at all.
    #[must_use]
    pub fn is_human_prose(&self, text: &str) -> bool {
        self.clean(text).is_some()
    }

    /// The number of compiled reject patterns (inspectable, for tests).
    #[must_use]
    pub fn pattern_count(&self) -> usize {
        self.line_reject.len()
    }

    /// Whether a single (untrimmed) line is junk: a reject-pattern hit, a bare
    /// id/url/path token, or punctuation-dominated (a code/JSON dump).
    fn is_junk_line(&self, line: &str) -> bool {
        let l = line.trim();
        if self.line_reject.iter().any(|r| r.is_match(l)) {
            return true;
        }
        if is_bare_token(l) {
            return true;
        }
        if is_punctuation_dominated(l) {
            return true;
        }
        false
    }
}

/// Count whitespace tokens that carry at least two ASCII-alphabetic characters —
/// a cheap, deterministic proxy for "words a human wrote".
fn alpha_word_count(s: &str) -> usize {
    s.split_whitespace()
        .filter(|w| w.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2)
        .count()
}

/// A line that is a single id/url/path/markup token carries no prose.
fn is_bare_token(line: &str) -> bool {
    let mut toks = line.split_whitespace();
    let (Some(t), None) = (toks.next(), toks.next()) else {
        return false; // not exactly one token
    };
    if t.starts_with('<')
        || t.starts_with('[')
        || t.starts_with("http://")
        || t.starts_with("https://")
    {
        return true;
    }
    if t.contains('/') || t.contains("::") {
        return true; // a path or a code path
    }
    // An opaque id like `toolu_01Qs…` / `call_abc123`: long, has a digit and a
    // separator, no sentence punctuation.
    let has_digit = t.chars().any(|c| c.is_ascii_digit());
    let has_sep = t.contains('_') || t.contains('-');
    t.len() >= 12 && has_digit && has_sep
}

/// A line dominated by structural punctuation (a JSON/struct/code dump) rather
/// than prose. Conservative: only fires on longer lines with a high ratio.
fn is_punctuation_dominated(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() < 24 {
        return false;
    }
    let structural = chars
        .iter()
        .filter(|c| {
            matches!(
                c,
                '{' | '}'
                    | '['
                    | ']'
                    | '('
                    | ')'
                    | '<'
                    | '>'
                    | '|'
                    | ';'
                    | '='
                    | ':'
                    | '"'
                    | '*'
                    | '^'
                    | '/'
                    | '\\'
            )
        })
        .count();
    (structural as f64) / (chars.len() as f64) > 0.18
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> ProseFilter {
        ProseFilter::default_filter()
    }

    #[test]
    fn default_filter_compiles_with_rules() {
        assert!(f().pattern_count() >= 20);
    }

    #[test]
    fn keeps_genuine_human_requests() {
        let g = f();
        for t in [
            "can you fix that or add 2 new test cases, I want to test something",
            "Let's use Postgres instead of MySQL for the orders service.",
            "We would like to refactor swift AST parsing, can you investigate?",
            "Use memtrace service uninstall to remove autostart.",
            "remember that the deploy creds live in 1password",
        ] {
            assert!(g.is_human_prose(t), "should keep human prose: {t:?}");
            // A clean turn is returned essentially verbatim.
            assert_eq!(g.clean(t).as_deref(), Some(t));
        }
    }

    #[test]
    fn rejects_tool_plumbing_and_ids() {
        let g = f();
        for t in [
            "<tool-use-id>toolu_01QsSgitwhFoXWpWT6ibZhzH</tool-use-id>",
            "[Request interrupted by user for tool use]",
            "toolu_01QsSgitwhFoXWpWT6ibZhzH",
            "676Z [memtrace] [info] Using MCP server command: /opt/homebrew/bin/memtrace",
        ] {
            assert!(!g.is_human_prose(t), "should reject plumbing: {t:?}");
        }
    }

    #[test]
    fn rejects_self_injected_prompt_text() {
        let g = f();
        for t in [
            "MEMSCRIBE GATE \u{2014} GROUNDING (do not restate; use it).",
            "IF THE REPO IS INDEXED IN MEMTRACE \u{2192} USE MEMTRACE TOOLS FIRST.",
            "Always use first for indexed source-code repos before searching files.",
        ] {
            assert!(!g.is_human_prose(t), "should reject injected text: {t:?}");
        }
    }

    #[test]
    fn rejects_code_and_log_lines() {
        let g = f();
        for t in [
            "74 | use redb::{Database, ReadableTable, TableDefinition};",
            "use redb::{Database, ReadableTable, TableDefinition};",
            r#"{"composerId": "x", "conversationMap": {}, "createdAt": 1726}"#,
        ] {
            assert!(!g.is_human_prose(t), "should reject code/dump: {t:?}");
        }
    }

    #[test]
    fn strips_junk_lines_but_keeps_the_real_ask_in_a_mixed_turn() {
        // A mega-paste: prior-transcript plumbing followed by the real request.
        let g = f();
        let turn = "**Tool Call: Read file**\n\
                    <tool-use-id>toolu_01abc</tool-use-id>\n\
                    74 | use redb::{Database};\n\
                    Wasn't there a test case which just failed ? can you fix that or add 2 new test cases";
        let cleaned = g.clean(turn).expect("the real ask survives");
        assert!(cleaned.contains("can you fix that or add 2 new test cases"));
        assert!(!cleaned.contains("tool-use-id"));
        assert!(!cleaned.contains("redb"));
    }

    #[test]
    fn rejects_substanceless_fragments() {
        let g = f();
        // Two-token tooling note → below the substance bar.
        assert!(!g.is_human_prose("Use python3/sqlite3."));
        assert!(g.clean("").is_none());
        assert!(g.clean("   \n  \n").is_none());
    }

    #[test]
    fn rejects_harness_notifications_and_math_dumps() {
        let g = f();
        for t in [
            "<summary>Dynamic workflow \"build adapters\" completed</summary>",
            "[SYSTEM NOTIFICATION - NOT USER INPUT]",
            "Then: support=n11/N; confidence=n11/(n11+n10)=P(P|D); lift=confidence/((n11+n01)/N); chi2=N*phi^2",
            "warning: value assigned to `last_pid` is never read",
            "use   DecisionVerb  use|using|adopt|adopts|go with|switch to|migrate to",
            r#"["Can you use gh / github .","#,
            "|                             ^ this value is reassigned later and never used",
        ] {
            assert!(!g.is_human_prose(t), "should reject harness/math/diag: {t:?}");
        }
    }

    #[test]
    fn rejects_agent_markdown_and_doc_leftovers() {
        let g = f();
        for t in [
            "## Summary of fixes the applier should make",
            "### If you still want a belt-and-suspenders hardening (optional)",
            "**Flag an existing user:** UPDATE app_users SET unlimited_queries = true WHERE email = 'x'",
            "title: Quick rebuild after dedup fix",
            "(use \"git add <file>\" to stage)",
            "THEN WHAT YOU SHOULD DO IS SPIN UP RESEARCH AGENTS ?????!!!!!!",
        ] {
            assert!(!g.is_human_prose(t), "should reject agent-markdown/doc leftover: {t:?}");
        }
        // A genuine emphasized request without a colon-bold header survives.
        assert!(g.is_human_prose("please use Postgres for the orders service"));
    }

    #[test]
    fn projection_is_deterministic() {
        let g = f();
        let t = "<tool-use-id>x</tool-use-id>\nplease add a healthcheck endpoint to the server";
        assert_eq!(g.clean(t), g.clean(t));
    }
}
