//! The adapter registry: assemble the set of enabled adapters and resolve one
//! by [`SourceKind`].

use memscribe_core::{SourceKind, TranscriptAdapter};

/// Every enabled adapter, in a stable order.
#[must_use]
#[allow(clippy::vec_init_then_push)] // pushes are cfg-gated; a vec! literal won't work
pub fn all_adapters() -> Vec<Box<dyn TranscriptAdapter>> {
    let mut v: Vec<Box<dyn TranscriptAdapter>> = Vec::new();
    #[cfg(feature = "claude_code")]
    v.push(Box::new(crate::claude_code::ClaudeCodeAdapter));
    #[cfg(feature = "codex")]
    v.push(Box::new(crate::codex::CodexAdapter));
    #[cfg(feature = "gemini")]
    v.push(Box::new(crate::gemini::GeminiAdapter));
    #[cfg(feature = "otel")]
    v.push(Box::new(crate::otel::OtelAdapter));
    #[cfg(feature = "cursor")]
    v.push(Box::new(crate::cursor::CursorAdapter));
    #[cfg(feature = "windsurf")]
    v.push(Box::new(crate::windsurf::WindsurfAdapter));
    #[cfg(feature = "zed")]
    v.push(Box::new(crate::zed::ZedAdapter));
    #[cfg(feature = "vscode")]
    v.push(Box::new(crate::vscode::VsCodeAdapter));
    #[cfg(feature = "copilot")]
    v.push(Box::new(crate::copilot::CopilotAdapter));
    v
}

/// Resolve the adapter for a given source, if its feature is enabled.
#[must_use]
pub fn adapter_for(kind: SourceKind) -> Option<Box<dyn TranscriptAdapter>> {
    match kind {
        #[cfg(feature = "claude_code")]
        SourceKind::ClaudeCode => Some(Box::new(crate::claude_code::ClaudeCodeAdapter)),
        #[cfg(feature = "codex")]
        SourceKind::Codex => Some(Box::new(crate::codex::CodexAdapter)),
        #[cfg(feature = "gemini")]
        SourceKind::Gemini => Some(Box::new(crate::gemini::GeminiAdapter)),
        #[cfg(feature = "otel")]
        SourceKind::Otel => Some(Box::new(crate::otel::OtelAdapter)),
        #[cfg(feature = "cursor")]
        SourceKind::Cursor => Some(Box::new(crate::cursor::CursorAdapter)),
        #[cfg(feature = "windsurf")]
        SourceKind::Windsurf => Some(Box::new(crate::windsurf::WindsurfAdapter)),
        #[cfg(feature = "zed")]
        SourceKind::Zed => Some(Box::new(crate::zed::ZedAdapter)),
        #[cfg(feature = "vscode")]
        SourceKind::VsCode => Some(Box::new(crate::vscode::VsCodeAdapter)),
        #[cfg(feature = "copilot")]
        SourceKind::Copilot => Some(Box::new(crate::copilot::CopilotAdapter)),
        _ => None,
    }
}
