//! The `memscribe` binary — the daemon and the toolbox (whitepaper §10).
#![forbid(unsafe_code)]

mod config;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use memscribe_core::{DefaultPipeline, PreparedNode, Redactor, Sink, SourceKind};
use memscribe_io::cursor_store::persistent::SqliteOffsetStore;
use memscribe_io::discover::find_transcripts;
use memscribe_io::tailer::LiveTailer;
use memscribe_sink::NdjsonSink;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Deterministic, zero-LLM transcript capture for AI coding agents.
#[derive(Parser, Debug)]
#[command(name = "memscribe", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the tailers + hook server; the steady-state capture daemon.
    Watch {
        /// Tools to watch (e.g. `claude,codex,gemini`). Empty = every adapter.
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
        /// Sink target: `ndjson`, `sqlite`, or `memdb`.
        #[arg(long, default_value = "ndjson")]
        sink: String,
        /// Where prepared nodes go (a file for `ndjson`/`sqlite`); `-` is stdout.
        #[arg(long, default_value = "-")]
        out: PathBuf,
        /// Directory roots to scan for transcripts (default: `$HOME`).
        #[arg(long = "root", value_name = "DIR")]
        roots: Vec<PathBuf>,
        /// Drain what already exists and exit, rather than tailing live.
        #[arg(long)]
        once: bool,
        /// Path to a `memscribe.toml` config.
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// The hook handler agents invoke (reads stdin, records, returns 0).
    Hook,
    /// One-shot parse a transcript to NDJSON (the workhorse for tests/debugging).
    Parse {
        /// The transcript file to parse.
        file: PathBuf,
        /// Force a specific tool adapter (e.g. `claude_code`).
        #[arg(long = "as", value_name = "TOOL")]
        source: Option<String>,
        /// Do not run the redaction pass (emit verbatim).
        #[arg(long)]
        no_redact: bool,
    },
    /// Re-run preparation over a historical session (file path).
    Replay {
        /// A transcript file to replay.
        target: PathBuf,
        /// Force a specific tool adapter.
        #[arg(long = "as", value_name = "TOOL")]
        source: Option<String>,
    },
    /// Run the conformance suite; with `--capture`, snapshot a live session.
    Verify {
        /// Snapshot this transcript into a new fixture instead of asserting.
        /// Pass the live/sample session file to capture.
        #[arg(long, value_name = "SESSION_FILE")]
        capture: Option<PathBuf>,
        /// Force a specific tool adapter for the captured session (e.g.
        /// `claude_code`). Inferred from the path when omitted.
        #[arg(long = "as", value_name = "TOOL")]
        source: Option<String>,
        /// The fixture base name to write (defaults to the session file stem).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// Also write the prepared nodes alongside the raw transcript
        /// (`<name>.nodes.ndjson`) so the captured corpus carries expected output.
        #[arg(long)]
        with_nodes: bool,
    },
    /// Show what the redaction pass would strip.
    Redact {
        /// The file to inspect.
        file: PathBuf,
        /// Elide all content (structure-only `--no-content` mode).
        #[arg(long)]
        no_content: bool,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Parse {
            file,
            source,
            no_redact,
        } => cmd_parse(&file, source.as_deref(), no_redact),
        Command::Replay { target, source } => cmd_parse(&target, source.as_deref(), false),
        Command::Redact { file, no_content } => cmd_redact(&file, no_content),
        Command::Hook => cmd_hook(),
        Command::Verify {
            capture,
            source,
            name,
            with_nodes,
        } => match capture {
            Some(session) => {
                cmd_verify_capture(&session, source.as_deref(), name.as_deref(), with_nodes)
            }
            None => cmd_verify(),
        },
        Command::Watch {
            tools,
            sink,
            out,
            roots,
            once,
            config,
        } => cmd_watch(&tools, &sink, &out, &roots, once, config.as_deref()),
    }
}

/// Resolve the tool adapter from an explicit `--as` flag or by inferring from
/// the file path.
fn resolve_source(source: Option<&str>, file: &Path) -> Result<SourceKind> {
    if let Some(s) = source {
        return SourceKind::parse(s).ok_or_else(|| anyhow!("unknown tool `{s}`"));
    }
    infer_source(file).ok_or_else(|| {
        anyhow!(
            "could not infer the tool from `{}`; pass --as <tool>",
            file.display()
        )
    })
}

/// Best-effort tool inference from a transcript path. Returns `None` when no
/// marker is recognizable (the caller decides whether that is fatal).
fn infer_source(file: &Path) -> Option<SourceKind> {
    let p = file.to_string_lossy().to_ascii_lowercase();
    let inferred = if p.contains(".codex") || p.contains("codex") {
        SourceKind::Codex
    } else if p.contains(".claude") || p.contains("claude") {
        SourceKind::ClaudeCode
    } else if p.contains(".gemini") || p.contains("gemini") {
        SourceKind::Gemini
    } else if p.contains("cursor") {
        SourceKind::Cursor
    } else if p.contains("windsurf") {
        SourceKind::Windsurf
    } else if p.contains("zed") {
        SourceKind::Zed
    } else if p.contains("copilot") {
        SourceKind::Copilot
    } else if p.contains("vscode") || p.contains("code") {
        SourceKind::VsCode
    } else if p.contains("otel") || p.ends_with(".ndjson") {
        SourceKind::Otel
    } else {
        return None;
    };
    Some(inferred)
}

fn cmd_parse(file: &Path, source: Option<&str>, no_redact: bool) -> Result<()> {
    let kind = resolve_source(source, file)?;
    let adapter = memscribe_adapters::adapter_for(kind)
        .ok_or_else(|| anyhow!("the `{kind}` adapter is not compiled into this build"))?;
    let records =
        memscribe_io::read_records(file).with_context(|| format!("reading {}", file.display()))?;

    let pipeline = if no_redact {
        DefaultPipeline::without_redaction()
    } else {
        DefaultPipeline::new()
    };
    let nodes = pipeline.run_records(adapter.as_ref(), &records);

    let mut sink = NdjsonSink::stdout();
    for n in &nodes {
        sink.emit(n)?;
    }
    sink.flush()?;
    Ok(())
}

fn cmd_redact(file: &Path, no_content: bool) -> Result<()> {
    let redactor = Redactor::with_default_patterns(no_content);
    let content =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let had_secret = redactor.contains_secret(&content);
    print!("{}", redactor.redact_text(&content));
    if had_secret {
        eprintln!("memscribe: redacted one or more secrets");
    }
    Ok(())
}

fn cmd_hook() -> Result<()> {
    // Read the hook payload, record it, and exit 0 immediately — never block the
    // agent, never invoke a model.
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf)?;
    if let Some(p) = memscribe_io::hook::record_hook(&buf) {
        tracing::debug!(
            event = ?p.payload.hook_event_name,
            transcript = ?p.transcript_path,
            "memscribe hook received"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

/// Parse every fixture under `fixtures/` and assert each yields a non-error node
/// stream, printing a per-tool PASS/FAIL table. Exits non-zero on any failure.
///
/// Full cross-tool conformance (golden equality, the §8.3 invariants) lives in
/// the testkit (`cargo test -p memscribe-testkit`); this is the fast, shellable
/// smoke summary the daemon ships with.
fn cmd_verify() -> Result<()> {
    let fixtures = memscribe_testkit::golden::fixtures_dir();
    if !fixtures.is_dir() {
        bail!("no fixtures directory at {}", fixtures.display());
    }

    // Aggregate per tool: (cases, ok-cases, total-nodes, sample failure).
    use std::collections::BTreeMap;
    #[derive(Default)]
    struct Tally {
        cases: usize,
        ok: usize,
        nodes: usize,
        first_error: Option<String>,
    }
    let mut by_tool: BTreeMap<String, Tally> = BTreeMap::new();

    // Walk fixtures/<tool>/<version>/<case>.jsonl deterministically.
    let mut tool_dirs: Vec<PathBuf> = std::fs::read_dir(&fixtures)
        .with_context(|| format!("reading {}", fixtures.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    tool_dirs.sort();

    for tool_dir in tool_dirs {
        let tool_slug = tool_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let kind = SourceKind::parse(&tool_slug);

        let files = find_transcripts(&tool_dir, &["jsonl", "json", "zst"]);
        for disc in files {
            let tally = by_tool.entry(tool_slug.clone()).or_default();
            tally.cases += 1;
            match verify_one(kind, &disc.path) {
                Ok(n) => {
                    tally.ok += 1;
                    tally.nodes += n;
                }
                Err(e) => {
                    if tally.first_error.is_none() {
                        tally.first_error = Some(format!("{}: {e}", disc.path.display()));
                    }
                }
            }
        }
    }

    // Render the table.
    println!(
        "{:<14} {:>6} {:>6} {:>8}  STATUS",
        "TOOL", "CASES", "OK", "NODES"
    );
    let mut any_fail = false;
    for (tool, t) in &by_tool {
        let pass = t.ok == t.cases && t.cases > 0;
        any_fail |= !pass;
        println!(
            "{:<14} {:>6} {:>6} {:>8}  {}",
            tool,
            t.cases,
            t.ok,
            t.nodes,
            if pass { "PASS" } else { "FAIL" }
        );
        if let Some(err) = &t.first_error {
            println!("    └─ first failure: {err}");
        }
    }

    if by_tool.is_empty() {
        bail!("no fixtures found under {}", fixtures.display());
    }
    if any_fail {
        bail!("verify: one or more tools FAILED conformance smoke");
    }
    println!("verify: all {} tool(s) PASS", by_tool.len());
    Ok(())
}

/// `verify --capture <session-file> --as <tool>` (whitepaper M6).
///
/// Snapshot a live/sample transcript into a NEW fixture so the corpus can grow
/// from real sessions: resolve the tool, parse the transcript to confirm it
/// yields a clean event stream (we never capture a broken sample), then copy the
/// raw transcript verbatim into `fixtures/<tool>/captured/<name>.jsonl`. With
/// `--with-nodes`, the prepared nodes are written alongside as
/// `<name>.nodes.ndjson` so the captured case carries its expected output.
fn cmd_verify_capture(
    session: &Path,
    source: Option<&str>,
    name: Option<&str>,
    with_nodes: bool,
) -> Result<()> {
    // Resolve the adapter the same way `parse` does (explicit `--as`, else infer).
    let kind = resolve_source(source, session)?;
    if memscribe_adapters::adapter_for(kind).is_none() {
        bail!("the `{kind}` adapter is not compiled into this build");
    }

    // Read the raw transcript bytes (verbatim — the fixture is the source of
    // truth) and the decompressed bytes for the conformance check.
    let raw =
        std::fs::read(session).with_context(|| format!("reading session {}", session.display()))?;
    let bytes = read_decompressed(session)?;

    // Confirm the sample is well-formed BEFORE we admit it to the corpus: the
    // event stream must satisfy the §8.3 invariants and every prepared node must
    // serialize. A broken sample is rejected rather than captured.
    verify_one(Some(kind), session).with_context(|| {
        format!(
            "refusing to capture {}: it does not pass conformance",
            session.display()
        )
    })?;

    // Choose the fixture name: explicit `--name`, else the session file stem.
    let base = match name {
        Some(n) => n.to_string(),
        None => session
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("could not derive a fixture name from {}", session.display()))?,
    };

    // fixtures/<tool>/captured/<name>.jsonl — a stable, discoverable home for
    // sessions captured from the wild, kept apart from the curated version dirs.
    // `MEMSCRIBE_FIXTURES_DIR` overrides the corpus root (used by tests to stay
    // hermetic — capture into a tempdir instead of the repo's `fixtures/`).
    let fixtures_root = std::env::var_os("MEMSCRIBE_FIXTURES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(memscribe_testkit::golden::fixtures_dir);
    let dest_dir = fixtures_root.join(kind.as_str()).join("captured");
    std::fs::create_dir_all(&dest_dir)
        .with_context(|| format!("creating fixture dir {}", dest_dir.display()))?;

    // Preserve the source extension (a `.zst` rollout stays compressed) so the
    // reader treats the captured fixture exactly like the original.
    let ext = session
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jsonl");
    let dest = dest_dir.join(format!("{base}.{ext}"));
    std::fs::write(&dest, &raw).with_context(|| format!("writing fixture {}", dest.display()))?;

    if with_nodes {
        // Prepare the nodes from the decompressed bytes and write them as NDJSON
        // next to the raw transcript.
        let nodes = memscribe_testkit::prepare_nodes(kind, &bytes, session);
        let node_count = nodes.len();
        let nodes_path = dest_dir.join(format!("{base}.nodes.ndjson"));
        let mut out = String::new();
        for n in &nodes {
            out.push_str(&serde_json::to_string(n).context("node failed to serialize")?);
            out.push('\n');
        }
        std::fs::write(&nodes_path, out)
            .with_context(|| format!("writing nodes {}", nodes_path.display()))?;
        println!(
            "captured {} → {} ({} node(s) → {})",
            session.display(),
            dest.display(),
            node_count,
            nodes_path.display()
        );
    } else {
        println!("captured {} → {}", session.display(), dest.display());
    }
    Ok(())
}

/// Parse one fixture and assert the whitepaper §8.3 conformance invariants on
/// the **event** stream (the real losslessness floor — note that some scenarios,
/// e.g. `tool_failure` and `no_commitment_marker`, correctly elevate *zero*
/// prepared nodes, so a non-empty node count is deliberately NOT required), plus
/// clean node serialization. Returns the prepared-node count on success.
fn verify_one(kind: Option<SourceKind>, path: &Path) -> Result<usize> {
    use memscribe_testkit::invariants::{
        check_determinism, check_lossless, check_monotonic_seq, check_unique_event_ids,
    };
    use memscribe_testkit::{count_nonblank_lines, parse_events, prepare_nodes};

    // Resolve the adapter: explicit tool slug from the directory, else infer.
    let kind = match kind {
        Some(k) => k,
        None => infer_source(path)
            .ok_or_else(|| anyhow!("could not resolve a tool for {}", path.display()))?,
    };
    if memscribe_adapters::adapter_for(kind).is_none() {
        bail!("the `{kind}` adapter is not compiled in");
    }

    // Read decompressed bytes so the testkit (which works on raw bytes) sees the
    // same content the one-shot reader would, including `.zst` rollouts.
    let bytes = read_decompressed(path)?;

    // Events: the lossless, deterministic, monotonic, deduped stream.
    let events = parse_events(kind, &bytes, path);
    check_lossless(count_nonblank_lines(&bytes), &events).map_err(|e| anyhow!(e))?;
    check_monotonic_seq(&events).map_err(|e| anyhow!(e))?;
    check_unique_event_ids(&events).map_err(|e| anyhow!(e))?;
    // Determinism: a second parse must be byte-identical.
    let events2 = parse_events(kind, &bytes, path);
    check_determinism(&events, &events2).map_err(|e| anyhow!(e))?;

    // Nodes: prepare and confirm every node serializes cleanly (round-trip floor).
    let nodes = prepare_nodes(kind, &bytes, path);
    for n in &nodes {
        serde_json::to_string(n).context("node failed to serialize")?;
    }
    Ok(nodes.len())
}

/// Read a transcript's bytes, transparently decompressing a `.zst` rollout.
fn read_decompressed(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        zstd::decode_all(&raw[..]).with_context(|| format!("decompressing {}", path.display()))
    } else {
        Ok(raw)
    }
}

// ---------------------------------------------------------------------------
// watch
// ---------------------------------------------------------------------------

/// The set of file extensions transcripts use across tools. Discovery is by
/// extension because adapter `discover()` is a stub in the initial model.
const TRANSCRIPT_EXTS: &[&str] = &["jsonl", "json", "ndjson", "zst"];

/// Run the live capture daemon: discover transcripts for the requested `tools`
/// under `roots`, tail them with the crash-safe [`LiveTailer`], prepare each
/// appended batch through the pipeline, and write the prepared nodes to `sink`.
///
/// Responds to Ctrl-C cleanly (the tail loop breaks on the next tick and the
/// sink is flushed) and never panics — discovery/IO problems are logged and the
/// affected file is skipped.
fn cmd_watch(
    tools: &[String],
    sink_kind: &str,
    out: &Path,
    roots: &[PathBuf],
    once: bool,
    config: Option<&Path>,
) -> Result<()> {
    // Load the config (if any). It feeds: (a) the tool set, (b) per-tool path
    // overrides + home/project_filter into a DiscoverCfg, (c) the gate rule
    // table, (d) the redaction patterns, and (e) the sink selection. A missing
    // section falls back to the compiled default, so a partial config is fine.
    let cfg = match config {
        Some(p) => {
            Some(Config::load(p).with_context(|| format!("loading config {}", p.display()))?)
        }
        None => None,
    };

    // Surface the parsed ingest cadence (retention/cadence is parsed-and-stored;
    // the tailer cadence wiring lands incrementally, so log it for now rather
    // than silently dropping it).
    if let Some(c) = &cfg {
        tracing::info!(
            poll_interval_secs = c.poll_interval().as_secs(),
            resume_from_offset = c.resume_from_offset(),
            backfill_days = c.backfill_days(),
            memdb_endpoint = c.memdb_endpoint().unwrap_or("-"),
            "memscribe.toml loaded"
        );
    }

    // Build the pipeline from config: a config-driven gate + redactor when given,
    // else the compiled defaults (redaction stays on either way).
    let pipeline = match &cfg {
        Some(c) => DefaultPipeline::new()
            .with_gate(c.build_gate()?)
            .with_redactor(Some(c.build_redactor()?)),
        None => DefaultPipeline::new(),
    };

    // Resolve the set of tools we will accept. Precedence: CLI `--tools`, then
    // the config's `[capture].tools`, then every compiled adapter.
    let tool_slugs: Vec<String> = if !tools.iter().any(|t| !t.trim().is_empty()) {
        cfg.as_ref()
            .map(|c| c.capture_tools().to_vec())
            .unwrap_or_default()
    } else {
        tools.to_vec()
    };
    let wanted = resolve_wanted_tools(&tool_slugs)?;

    // The discovery config the adapters consult (per-tool overrides + home +
    // project_filter). When no config is present this is the default cfg.
    let disc_cfg = cfg.as_ref().map(Config::discover_cfg).unwrap_or_default();

    // Did the caller scope the scan with explicit `--root`s? If so, those roots
    // are AUTHORITATIVE: we walk exactly them and do NOT also consult each
    // adapter's home-based `discover()`. Mixing the two would let a `--root
    // <tmp>` scan silently pick up the user's real `~/.claude` sessions — which
    // breaks the scoping contract (an empty `--root` must discover nothing) and
    // makes the command non-hermetic. When no `--root` is given we fall back to
    // the config/`$HOME` home dir AND the adapters' own discovery (the latter is
    // what consumes the config's per-tool path overrides like `CODEX_HOME`).
    let roots_explicit = !roots.is_empty();
    let roots: Vec<PathBuf> = if roots_explicit {
        roots.to_vec()
    } else {
        vec![disc_cfg.home_dir()]
    };

    // Discover candidate transcripts and bucket each to a tool.
    //   (1) The extension walk over `roots` — always run; with explicit roots
    //       this is the *only* source, so the scan stays scoped to them.
    //   (2) Each wanted adapter's own `discover(&disc_cfg)` — only when roots
    //       were NOT given explicitly, so the home-based product paths (and the
    //       config's per-tool overrides) are honored in the default mode.
    let mut targets: Vec<(SourceKind, PathBuf)> = Vec::new();
    for root in &roots {
        for disc in find_transcripts(root, TRANSCRIPT_EXTS) {
            if let Some(kind) = infer_source(&disc.path) {
                if wanted.contains(&kind) {
                    targets.push((kind, disc.path));
                }
            }
        }
    }
    if !roots_explicit {
        for kind in &wanted {
            if let Some(adapter) = memscribe_adapters::adapter_for(*kind) {
                for handle in adapter.discover(&disc_cfg) {
                    targets.push((handle.source, handle.path));
                }
            }
        }
    }
    // Deterministic order, deduped by path (the same file is never tailed twice
    // even if two roots overlap). `SourceKind` is not `Ord`, so key on the path.
    targets.sort_by(|a, b| a.1.cmp(&b.1));
    targets.dedup_by(|a, b| a.1 == b.1);

    let tool_list: Vec<&str> = wanted.iter().map(SourceKind::as_str).collect();
    eprintln!(
        "memscribe watch: {} tool(s) [{}], {} root(s), {} transcript(s) discovered",
        wanted.len(),
        tool_list.join(","),
        roots.len(),
        targets.len()
    );

    // Sink selection: the config's `[sink]` target/path, else the CLI flags.
    let (sink_kind, sink_out): (String, PathBuf) = match &cfg {
        Some(c) => (
            c.sink_target().to_string(),
            c.sink_out_path().unwrap_or_else(|| out.to_path_buf()),
        ),
        None => (sink_kind.to_string(), out.to_path_buf()),
    };
    let mut sink = build_sink(&sink_kind, &sink_out)?;

    if once {
        // One-shot: drain everything that already exists once, then exit. This
        // is the documented simplification for the initial model.
        return watch_once(&targets, &pipeline, sink.as_mut());
    }

    watch_live(&targets, &pipeline, sink.as_mut())
}

/// The wanted-tool set from `--tools` (empty = every adapter that is compiled).
fn resolve_wanted_tools(tools: &[String]) -> Result<Vec<SourceKind>> {
    if tools.is_empty() {
        return Ok(memscribe_adapters::all_adapters()
            .iter()
            .map(|a| a.source_kind())
            .collect());
    }
    let mut out = Vec::new();
    for t in tools {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        let kind = SourceKind::parse(t).ok_or_else(|| anyhow!("unknown tool `{t}`"))?;
        if memscribe_adapters::adapter_for(kind).is_none() {
            bail!("the `{kind}` adapter is not compiled into this build");
        }
        if !out.contains(&kind) {
            out.push(kind);
        }
    }
    if out.is_empty() {
        bail!("no valid tools requested");
    }
    Ok(out)
}

/// Build the chosen sink. `ndjson` (default) writes one JSON node per line to a
/// file or stdout; `sqlite`/`memdb` are recognized but gated on their features.
fn build_sink(kind: &str, out: &Path) -> Result<Box<dyn Sink>> {
    match kind {
        "ndjson" => {
            if out == Path::new("-") {
                Ok(Box::new(NdjsonSink::stdout()))
            } else {
                Ok(Box::new(NdjsonSink::file(out).with_context(|| {
                    format!("opening ndjson sink at {}", out.display())
                })?))
            }
        }
        "sqlite" => {
            if out == Path::new("-") {
                bail!("the `sqlite` sink needs a file path; pass `--out <file.sqlite>`");
            }
            Ok(Box::new(memscribe_sink::SqliteSink::open(out).map_err(
                |e| anyhow!("opening sqlite sink at {}: {e}", out.display()),
            )?))
        }
        "memdb" => bail!(
            "the `memdb` sink is not compiled into this build (Memtrace enables it via the \
             `memdb-sink` feature)"
        ),
        other => bail!("unknown sink `{other}`; expected `ndjson`, `sqlite`, or `memdb`"),
    }
}

/// One-shot: read every existing transcript fully and emit its prepared nodes
/// through the (possibly config-driven) `pipeline`.
fn watch_once(
    targets: &[(SourceKind, PathBuf)],
    pipeline: &DefaultPipeline,
    sink: &mut dyn Sink,
) -> Result<()> {
    let mut total = 0usize;
    for (kind, path) in targets {
        match prepare_file(*kind, path, pipeline) {
            Ok(nodes) => {
                for n in &nodes {
                    sink.emit(n)?;
                }
                total += nodes.len();
                tracing::info!(tool = %kind, path = %path.display(), nodes = nodes.len(), "drained");
            }
            Err(e) => {
                tracing::warn!(tool = %kind, path = %path.display(), error = %e, "skipping transcript");
            }
        }
    }
    sink.flush()?;
    eprintln!(
        "memscribe watch --once: emitted {total} node(s) from {} transcript(s)",
        targets.len()
    );
    Ok(())
}

/// Live tailing: register every discovered transcript with one [`LiveTailer`]
/// (backed by a persistent SQLite cursor so restarts resume), drain what already
/// exists, then loop emitting prepared nodes for each appended batch until
/// Ctrl-C. Each batch is routed to its file's adapter by path.
fn watch_live(
    targets: &[(SourceKind, PathBuf)],
    pipeline: &DefaultPipeline,
    sink: &mut dyn Sink,
) -> Result<()> {
    // A path → tool map so a tailer batch (which carries provenance paths) is
    // routed to the right adapter.
    use std::collections::HashMap;
    let by_path: HashMap<PathBuf, SourceKind> =
        targets.iter().map(|(k, p)| (p.clone(), *k)).collect();

    // Persistent offset cursor under the user's state dir, so a restart resumes
    // exactly where it left off (zero loss, zero dup — whitepaper §8.5).
    let store = open_cursor_store()?;
    let mut tailer = LiveTailer::new(store, Duration::from_millis(200))
        .map_err(|e| anyhow!("creating the live tailer: {e}"))?;
    tailer
        .watch_paths(by_path.keys())
        .map_err(|e| anyhow!("registering transcripts to watch: {e}"))?;

    // Clean Ctrl-C: flip a flag the tail loop checks on each tick, then break.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        // Best-effort: if a handler is already installed (e.g. in tests), don't
        // make that fatal.
        let _ = ctrlc::set_handler(move || {
            stop.store(true, Ordering::SeqCst);
        });
    }

    // Drain pre-existing content once before going live.
    let pre = tailer.poll_existing();
    emit_batch(&pre, &by_path, pipeline, sink)?;
    sink.flush()?;
    eprintln!(
        "memscribe watch: tailing {} transcript(s); press Ctrl-C to stop",
        by_path.len()
    );

    // The blocking tail loop. We drive `poll` directly (rather than `run`) so the
    // `stop` flag is checked on EVERY tick — including idle timeouts, which `run`
    // skips. That is what makes Ctrl-C responsive on an otherwise-quiet tree:
    // without it, an idle watcher would never observe the stop signal until the
    // next append. A short tick keeps shutdown latency at one tick.
    let tick = Duration::from_millis(200);
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match tailer.poll(tick) {
            // `Some(empty)` is a timeout — loop back and re-check `stop`.
            Some(batch) if batch.is_empty() => continue,
            Some(batch) => {
                emit_batch(&batch, &by_path, pipeline, sink)?;
                sink.flush()?;
            }
            // The watcher shut down (sender dropped) — end of stream.
            None => break,
        }
    }

    sink.flush()?;
    eprintln!("memscribe watch: stopped cleanly");
    Ok(())
}

/// Open the persistent cursor store under the OS state dir, creating parents.
fn open_cursor_store() -> Result<SqliteOffsetStore> {
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join(".local/state/memscribe");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating cursor dir {}", dir.display()))?;
    let db = dir.join("cursors.sqlite");
    SqliteOffsetStore::open(&db).map_err(|e| anyhow!("opening cursor store {}: {e}", db.display()))
}

/// Group a tailer batch by source file, route each group to its adapter, and
/// emit the prepared nodes. A record whose file we don't have a tool for is
/// skipped (logged), never fatal.
fn emit_batch(
    batch: &[memscribe_core::RawRecord],
    by_path: &std::collections::HashMap<PathBuf, SourceKind>,
    pipeline: &DefaultPipeline,
    sink: &mut dyn Sink,
) -> Result<()> {
    use std::collections::BTreeMap;
    // Preserve per-file order; BTreeMap keeps deterministic file ordering.
    let mut grouped: BTreeMap<PathBuf, Vec<memscribe_core::RawRecord>> = BTreeMap::new();
    for rec in batch {
        grouped
            .entry(rec.location.file.clone())
            .or_default()
            .push(rec.clone());
    }
    for (file, recs) in grouped {
        let kind = match by_path.get(&file).copied().or_else(|| infer_source(&file)) {
            Some(k) => k,
            None => {
                tracing::warn!(path = %file.display(), "no adapter for tailed file; skipping batch");
                continue;
            }
        };
        let Some(adapter) = memscribe_adapters::adapter_for(kind) else {
            continue;
        };
        let nodes = pipeline.run_records(adapter.as_ref(), &recs);
        for n in &nodes {
            sink.emit(n)?;
        }
    }
    Ok(())
}

/// Read a whole transcript file and prepare its nodes through `pipeline`
/// (redaction **on** — the safe default for anything that lands in a sink).
fn prepare_file(
    kind: SourceKind,
    path: &Path,
    pipeline: &DefaultPipeline,
) -> Result<Vec<PreparedNode>> {
    let adapter = memscribe_adapters::adapter_for(kind)
        .ok_or_else(|| anyhow!("the `{kind}` adapter is not compiled in"))?;
    let records =
        memscribe_io::read_records(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(pipeline.run_records(adapter.as_ref(), &records))
}
