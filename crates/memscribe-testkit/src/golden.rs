//! Golden-file fixture resolution and comparison (whitepaper §8.1).
//!
//! Layout:
//! ```text
//! fixtures/<tool>/<version>/<case>.jsonl            # input
//! fixtures-expected/<tool>/<version>/<case>.events.json   # normalized events
//! fixtures-expected/<tool>/<version>/<case>.nodes.json    # prepared nodes
//! ```

use std::path::{Path, PathBuf};

/// The workspace root (two levels up from this crate's manifest dir).
#[must_use]
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The `fixtures/` directory.
#[must_use]
pub fn fixtures_dir() -> PathBuf {
    workspace_root().join("fixtures")
}

/// The `fixtures-expected/` directory.
#[must_use]
pub fn fixtures_expected_dir() -> PathBuf {
    workspace_root().join("fixtures-expected")
}

/// A single golden case: the input transcript and its expected outputs.
#[derive(Debug, Clone)]
pub struct GoldenCase {
    /// The tool slug (e.g. `claude_code`).
    pub tool: String,
    /// The tool version slug (e.g. `2.1`).
    pub version: String,
    /// The case name.
    pub case: String,
}

impl GoldenCase {
    /// Construct a golden case descriptor.
    pub fn new(
        tool: impl Into<String>,
        version: impl Into<String>,
        case: impl Into<String>,
    ) -> Self {
        GoldenCase {
            tool: tool.into(),
            version: version.into(),
            case: case.into(),
        }
    }

    /// The input transcript path.
    #[must_use]
    pub fn input_path(&self) -> PathBuf {
        fixtures_dir()
            .join(&self.tool)
            .join(&self.version)
            .join(format!("{}.jsonl", self.case))
    }

    /// The expected normalized-events path.
    #[must_use]
    pub fn expected_events_path(&self) -> PathBuf {
        fixtures_expected_dir()
            .join(&self.tool)
            .join(&self.version)
            .join(format!("{}.events.json", self.case))
    }

    /// The expected prepared-nodes path.
    #[must_use]
    pub fn expected_nodes_path(&self) -> PathBuf {
        fixtures_expected_dir()
            .join(&self.tool)
            .join(&self.version)
            .join(format!("{}.nodes.json", self.case))
    }

    /// Read the input transcript bytes.
    ///
    /// # Errors
    /// Returns an [`std::io::Error`] if the fixture is missing.
    pub fn read_input(&self) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.input_path())
    }
}

/// Discover every `*.jsonl` fixture under `fixtures/`, returning golden-case
/// descriptors. Useful for a data-driven test that iterates all cases.
#[must_use]
pub fn discover_cases() -> Vec<GoldenCase> {
    let root = fixtures_dir();
    let mut cases = Vec::new();
    let Ok(tools) = std::fs::read_dir(&root) else {
        return cases;
    };
    for tool in tools.flatten() {
        if !tool.path().is_dir() {
            continue;
        }
        let tool_name = tool.file_name().to_string_lossy().to_string();
        let Ok(versions) = std::fs::read_dir(tool.path()) else {
            continue;
        };
        for version in versions.flatten() {
            if !version.path().is_dir() {
                continue;
            }
            let version_name = version.file_name().to_string_lossy().to_string();
            let Ok(files) = std::fs::read_dir(version.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        cases.push(GoldenCase::new(&tool_name, &version_name, stem));
                    }
                }
            }
        }
    }
    cases.sort_by(|a, b| {
        (a.tool.clone(), a.version.clone(), a.case.clone()).cmp(&(
            b.tool.clone(),
            b.version.clone(),
            b.case.clone(),
        ))
    });
    cases
}
