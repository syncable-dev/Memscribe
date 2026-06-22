//! The `memscribe.toml` config loader (whitepaper §10).
//!
//! Parses a [`Config`] from the on-disk TOML and resolves it into the concrete
//! runtime knobs the pipeline already exposes — there is no bespoke runtime
//! state here, only a deterministic mapping onto existing types:
//!
//! | TOML section          | runtime type                                  |
//! |-----------------------|-----------------------------------------------|
//! | `[capture]`           | tool set + [`DiscoverCfg`] (`home`, `project_filter`) |
//! | `[tools.*.overrides]` | [`DiscoverCfg::overrides`]                    |
//! | `[[gate.rules]]`      | [`CommitmentGate::from_triples`]              |
//! | `[redact]` / patterns | [`Redactor::from_patterns`] / `no_content`    |
//! | `[ingest]`            | parsed-and-stored cadence (may be unused yet) |
//! | `[sink]`              | sink target + path/endpoint                   |
//!
//! Every section is optional: a value left out falls back to the compiled
//! default (`CommitmentGate::default_table`, `Redactor::default`, the NDJSON
//! sink), so a minimal or empty config is valid and changes nothing.
//!
//! The schema mirrors the committed `memscribe.example.toml` one-to-one; that
//! file is the conformance fixture for [`Config::load`] (see the tests).

use anyhow::{Context, Result};
use memscribe_core::{CommitmentGate, DiscoverCfg, MarkerCategory, Redactor};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The parsed `memscribe.toml`. All sections are optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which tools to tail and where their transcripts live.
    #[serde(default)]
    pub capture: CaptureCfg,
    /// Per-tool path overrides, keyed by tool slug then env-var name.
    #[serde(default)]
    pub tools: HashMap<String, ToolCfg>,
    /// The commitment-marker gate rule table (replaces the default when present).
    #[serde(default)]
    pub gate: Option<GateCfg>,
    /// The redaction pass configuration.
    #[serde(default)]
    pub redact: Option<RedactCfg>,
    /// Retention / ingest cadence (parsed and stored; may be unused).
    #[serde(default)]
    pub ingest: IngestCfg,
    /// Where prepared nodes go.
    #[serde(default)]
    pub sink: SinkCfg,
}

/// The `[capture]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureCfg {
    /// The adapter slugs to enable (e.g. `["claude_code", "codex"]`). Empty =
    /// "use the CLI `--tools` value / every adapter".
    #[serde(default)]
    pub tools: Vec<String>,
    /// Restrict discovery to a single project root (`DiscoverCfg.project_filter`).
    #[serde(default)]
    pub project_filter: Option<PathBuf>,
    /// Override `$HOME` for discovery (`DiscoverCfg.home`).
    #[serde(default)]
    pub home: Option<PathBuf>,
}

/// A `[tools.<slug>]` table: currently just per-tool path overrides.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCfg {
    /// Native-env-var → path overrides (e.g. `CODEX_HOME`, `CLAUDE_CONFIG_DIR`).
    #[serde(default)]
    pub overrides: HashMap<String, PathBuf>,
}

/// The `[gate]` section: a replacement commitment-marker rule table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateCfg {
    /// The `[[gate.rules]]` array.
    #[serde(default)]
    pub rules: Vec<GateRuleCfg>,
}

/// One `[[gate.rules]]` entry: the `(id, category, pattern)` triple
/// [`CommitmentGate::from_triples`] consumes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRuleCfg {
    /// The rule id (e.g. `decision_verb.use`).
    pub id: String,
    /// The marker category (snake_case, matching [`MarkerCategory`]).
    pub category: MarkerCategory,
    /// The case-insensitive regex pattern.
    pub pattern: String,
}

/// The `[redact]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactCfg {
    /// Structure-only mode: elide ALL verbatim text (the `--no-content` flag).
    #[serde(default)]
    pub no_content: bool,
    /// The `[[redact.patterns]]` array (replaces the default set when present).
    #[serde(default)]
    pub patterns: Vec<RedactPatternCfg>,
}

/// One `[[redact.patterns]]` entry: a `(label, pattern)` pair.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactPatternCfg {
    /// The label, surfaced as `[REDACTED:<label>]`.
    pub label: String,
    /// The regex pattern to strip.
    pub pattern: String,
}

/// The `[ingest]` section: retention / cadence. Parsed and stored even though
/// the current daemon does not yet read every field (the loader is the contract;
/// wiring each knob is incremental).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngestCfg {
    /// How often the tailer re-scans for new bytes (seconds).
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Resume each file from its last byte offset instead of re-reading.
    #[serde(default = "default_true")]
    pub resume_from_offset: bool,
    /// Cold-start lookback window in days; `0`/omitted = full history.
    #[serde(default)]
    pub backfill_days: u64,
}

impl Default for IngestCfg {
    fn default() -> Self {
        IngestCfg {
            poll_interval_secs: default_poll_interval_secs(),
            resume_from_offset: true,
            backfill_days: 0,
        }
    }
}

fn default_poll_interval_secs() -> u64 {
    5
}
fn default_true() -> bool {
    true
}

/// The `[sink]` section.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkCfg {
    /// `ndjson` (default) | `sqlite` | `memdb`.
    #[serde(default)]
    pub target: Option<String>,
    /// `[sink.ndjson]`.
    #[serde(default)]
    pub ndjson: Option<NdjsonSinkCfg>,
    /// `[sink.sqlite]`.
    #[serde(default)]
    pub sqlite: Option<SqliteSinkCfg>,
    /// `[sink.memdb]`.
    #[serde(default)]
    pub memdb: Option<MemdbSinkCfg>,
}

/// `[sink.ndjson]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NdjsonSinkCfg {
    /// Output path; omit for stdout.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// `[sink.sqlite]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteSinkCfg {
    /// The SQLite database path.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// `[sink.memdb]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemdbSinkCfg {
    /// The MemDB endpoint (`host:port`).
    #[serde(default)]
    pub endpoint: Option<String>,
}

impl Config {
    /// Parse a [`Config`] from a TOML file on disk.
    ///
    /// # Errors
    /// Returns an error if the file cannot be read or is not valid TOML for this
    /// schema.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Parse a [`Config`] from a TOML string (the unit-testable core of
    /// [`Self::load`]).
    ///
    /// # Errors
    /// Returns the underlying TOML error on a schema mismatch.
    pub fn parse_str(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text).context("invalid memscribe.toml")?;
        Ok(cfg)
    }

    /// Build the [`DiscoverCfg`] this config implies: `home`, `project_filter`,
    /// and the union of every tool's `overrides`.
    #[must_use]
    pub fn discover_cfg(&self) -> DiscoverCfg {
        let mut overrides: HashMap<String, PathBuf> = HashMap::new();
        for tool in self.tools.values() {
            for (k, v) in &tool.overrides {
                overrides.insert(k.clone(), v.clone());
            }
        }
        DiscoverCfg {
            home: self.capture.home.clone(),
            overrides,
            project_filter: self.capture.project_filter.clone(),
        }
    }

    /// Build the commitment gate: the config's `[[gate.rules]]` if any are given,
    /// otherwise the compiled default table.
    ///
    /// # Errors
    /// Returns an error if a configured pattern fails to compile.
    pub fn build_gate(&self) -> Result<CommitmentGate> {
        match &self.gate {
            Some(g) if !g.rules.is_empty() => {
                let triples = g
                    .rules
                    .iter()
                    .map(|r| (r.id.clone(), r.category, r.pattern.clone()));
                CommitmentGate::from_triples(triples)
                    .context("compiling a configured [[gate.rules]] pattern")
            }
            _ => Ok(CommitmentGate::default_table()),
        }
    }

    /// Build the redactor implied by `[redact]`. Custom `[[redact.patterns]]`
    /// replace the default set; with none given the default patterns are used.
    /// `no_content` is honored either way. Returns `None` only if a future config
    /// disables redaction (currently redaction is always on).
    ///
    /// # Errors
    /// Returns an error if a configured pattern fails to compile.
    pub fn build_redactor(&self) -> Result<Redactor> {
        match &self.redact {
            Some(r) if !r.patterns.is_empty() => {
                let pairs = r
                    .patterns
                    .iter()
                    .map(|p| (p.label.clone(), p.pattern.clone()));
                Redactor::from_patterns(pairs, r.no_content)
                    .context("compiling a configured [[redact.patterns]] pattern")
            }
            Some(r) => Ok(Redactor::with_default_patterns(r.no_content)),
            None => Ok(Redactor::default()),
        }
    }

    /// The configured sink target, defaulting to `ndjson`.
    #[must_use]
    pub fn sink_target(&self) -> &str {
        self.sink.target.as_deref().unwrap_or("ndjson")
    }

    /// The configured output path for the selected sink, if the config names one
    /// (`[sink.ndjson].path` or `[sink.sqlite].path`). `None` means "use the CLI
    /// `--out` value".
    #[must_use]
    pub fn sink_out_path(&self) -> Option<PathBuf> {
        match self.sink_target() {
            "sqlite" => self.sink.sqlite.as_ref().and_then(|s| s.path.clone()),
            _ => self.sink.ndjson.as_ref().and_then(|s| s.path.clone()),
        }
    }

    /// The tool slugs the config selects, if `[capture].tools` is non-empty.
    #[must_use]
    pub fn capture_tools(&self) -> &[String] {
        &self.capture.tools
    }

    /// The tailer poll interval implied by `[ingest].poll_interval_secs`.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.ingest.poll_interval_secs)
    }

    /// Whether `[ingest].resume_from_offset` is set (resume vs. re-read).
    #[must_use]
    pub fn resume_from_offset(&self) -> bool {
        self.ingest.resume_from_offset
    }

    /// The cold-start backfill window in days (`[ingest].backfill_days`); `0`
    /// means "ingest the full available history".
    #[must_use]
    pub fn backfill_days(&self) -> u64 {
        self.ingest.backfill_days
    }

    /// The configured MemDB endpoint, when the sink target is `memdb`
    /// (`[sink.memdb].endpoint`). Surfaced so a future `memdb` sink can consume
    /// it; `None` otherwise.
    #[must_use]
    pub fn memdb_endpoint(&self) -> Option<&str> {
        if self.sink_target() == "memdb" {
            self.sink.memdb.as_ref().and_then(|m| m.endpoint.as_deref())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed example config must parse with this schema — it is the
    /// loader's conformance fixture.
    #[test]
    fn example_toml_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf();
        let example = root.join("memscribe.example.toml");
        let cfg = Config::load(&example).expect("memscribe.example.toml must parse");

        // Spot-check the round-trip against the committed values.
        assert_eq!(cfg.capture_tools(), ["claude_code", "codex", "gemini"]);
        assert_eq!(cfg.sink_target(), "ndjson");

        // The example ships the 8 default gate rules and the default redact set.
        let gate = cfg.build_gate().expect("example gate rules compile");
        assert!(gate.rule_count() >= 8);
        let redactor = cfg
            .build_redactor()
            .expect("example redact patterns compile");
        assert!(redactor.contains_secret("sk-ant-AAAAAAAAAAAAAAAAAAAA"));

        // Per-tool overrides land in the DiscoverCfg.
        let disc = cfg.discover_cfg();
        assert!(disc.overrides.contains_key("CODEX_HOME"));
        assert!(disc.overrides.contains_key("CLAUDE_CONFIG_DIR"));

        // Ingest cadence parses.
        assert_eq!(cfg.ingest.poll_interval_secs, 5);
        assert!(cfg.ingest.resume_from_offset);
    }

    #[test]
    fn empty_config_is_all_defaults() {
        let cfg = Config::parse_str("").expect("empty config parses");
        assert!(cfg.capture_tools().is_empty());
        assert_eq!(cfg.sink_target(), "ndjson");
        assert!(cfg.sink_out_path().is_none());
        // Default gate + default redactor.
        assert_eq!(cfg.build_gate().unwrap().rule_count(), 8);
        assert!(cfg
            .build_redactor()
            .unwrap()
            .contains_secret("sk-ant-AAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn custom_gate_replaces_the_default_table() {
        let text = r#"
[[gate.rules]]
id = "custom.banana"
category = "decision_verb"
pattern = "banana"
"#;
        let cfg = Config::parse_str(text).unwrap();
        let gate = cfg.build_gate().unwrap();
        assert_eq!(gate.rule_count(), 1);
        assert!(gate.admits("we will use banana"));
        assert!(!gate.admits("let's go with redis"));
    }

    #[test]
    fn custom_redaction_pattern_is_applied() {
        let text = r#"
[redact]
no_content = false

[[redact.patterns]]
label = "banana_token"
pattern = "BANANA-[0-9]{4}"
"#;
        let cfg = Config::parse_str(text).unwrap();
        let r = cfg.build_redactor().unwrap();
        let out = r.redact_text("here is BANANA-1234 in the log");
        assert!(out.contains("[REDACTED:banana_token]"));
        // The default patterns are REPLACED, so a default secret survives.
        assert!(!r.contains_secret("AKIAABCDEFGHIJKLMNOP"));
    }

    #[test]
    fn invalid_pattern_surfaces_as_error() {
        let text = r#"
[[gate.rules]]
id = "bad"
category = "imperative"
pattern = "("
"#;
        let cfg = Config::parse_str(text).unwrap();
        assert!(cfg.build_gate().is_err());
    }

    #[test]
    fn sink_path_selection_follows_target() {
        let text = r#"
[sink]
target = "sqlite"

[sink.sqlite]
path = "/tmp/x.db"
"#;
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.sink_target(), "sqlite");
        assert_eq!(cfg.sink_out_path().unwrap(), PathBuf::from("/tmp/x.db"));
    }

    #[test]
    fn unknown_category_is_rejected() {
        let text = r#"
[[gate.rules]]
id = "x"
category = "not_a_category"
pattern = "x"
"#;
        assert!(Config::parse_str(text).is_err());
    }
}
