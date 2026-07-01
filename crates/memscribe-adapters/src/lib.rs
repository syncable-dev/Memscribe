//! # memscribe-adapters
//!
//! Per-tool transcript adapters. Each tool implements
//! [`memscribe_core::TranscriptAdapter`]: where its logs live, and how to turn
//! one raw record into normalized [`memscribe_core::CaptureEvent`]s. Parsers are
//! **version-tolerant** — they route anything unrecognized to
//! [`memscribe_core::EventKind::Unknown`] rather than failing — and **must never
//! panic** (every parser has a fuzz target).
//!
//! Adapters are behind feature flags so a consumer can compile only the tools it
//! needs. The [`registry`] assembles the set of enabled adapters.
#![forbid(unsafe_code)]

pub mod util;

#[cfg(feature = "claude_code")]
pub mod claude_code;
#[cfg(feature = "codex")]
pub mod codex;
#[cfg(feature = "copilot")]
pub mod copilot;
#[cfg(feature = "cursor")]
pub mod cursor;
#[cfg(feature = "gemini")]
pub mod gemini;
#[cfg(feature = "hermes")]
pub mod hermes;
#[cfg(feature = "opencode")]
pub mod opencode;
#[cfg(feature = "otel")]
pub mod otel;
#[cfg(feature = "vscode")]
pub mod vscode;
#[cfg(feature = "windsurf")]
pub mod windsurf;
#[cfg(feature = "zed")]
pub mod zed;

pub mod registry;

pub use registry::{adapter_for, all_adapters};
