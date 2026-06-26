//! Languageness (L0) — a deterministic "is this natural language, or garbage?"
//! score. Catches hashes, base64, stack traces, JSON/log blobs, and gibberish
//! that slip past line-level hygiene and look superficially like prose. Zero-LLM,
//! zero-corpus: pure character/token statistics (the classical readability /
//! garbage-detection heuristics), not a trained model.

/// A 0..=1 languageness score — higher = more like natural English prose. Built
/// from four signals (alphabetic ratio, vowel ratio among letters, word-like
/// token ratio, symbol density) plus a hard penalty for an over-long opaque token
/// (a hash / base64 / id). Code-ish decision text with identifiers still scores
/// well because snake_case/CamelCase tokens carry vowels and the prose around
/// them is word-like.
#[must_use]
pub fn languageness(text: &str) -> f32 {
    let s = text.trim();
    if s.is_empty() {
        return 0.0;
    }
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len() as f32;
    let letters = chars.iter().filter(|c| c.is_ascii_alphabetic()).count() as f32;
    let vowels = chars
        .iter()
        .filter(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
        .count() as f32;
    let symbols = chars
        .iter()
        .filter(|c| !c.is_ascii_alphanumeric() && !c.is_whitespace())
        .count() as f32;

    let alpha_ratio = letters / total;
    let vowel_ratio = if letters > 0.0 { vowels / letters } else { 0.0 };
    let symbol_ratio = symbols / total;

    let tokens: Vec<&str> = s.split_whitespace().collect();
    let token_count = tokens.len().max(1) as f32;
    // A "word-like" token: not absurdly long, and carries a vowel (or is a short
    // function word / number) — i.e. pronounceable rather than an opaque blob.
    let word_like = tokens
        .iter()
        .filter(|t| {
            let len = t.chars().count();
            let has_vowel = t
                .chars()
                .any(|c| matches!(c.to_ascii_lowercase(), 'a' | 'e' | 'i' | 'o' | 'u' | 'y'));
            len <= 3 || (len <= 32 && has_vowel)
        })
        .count() as f32;
    let word_like_ratio = word_like / token_count;
    let longest = tokens.iter().map(|t| t.chars().count()).max().unwrap_or(0);

    let mut score = 1.0_f32;
    if alpha_ratio < 0.55 {
        score -= (0.55 - alpha_ratio) * 1.2;
    }
    if vowel_ratio < 0.30 {
        score -= (0.30 - vowel_ratio) * 1.5;
    }
    score -= (1.0 - word_like_ratio) * 0.6;
    if symbol_ratio > 0.30 {
        score -= (symbol_ratio - 0.30) * 1.2;
    }
    if longest >= 40 {
        score -= 0.4; // an opaque hash/base64/id token dominates
    }
    score.clamp(0.0, 1.0)
}

/// Clear non-language garbage (a hash, base64 blob, log line, JSON soup) — below
/// the languageness floor. Tuned conservatively so prose with code identifiers
/// stays above it; only obvious garbage is rejected.
#[must_use]
pub fn is_garbage(text: &str) -> bool {
    languageness(text) < 0.35
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_decisions_score_high() {
        for s in [
            "use Postgres instead of MySQL for the orders service",
            "drop cross-compilation targets (ort-sys incompatible)",
            "delete_overlays_by_filter primitive",
            "MEMTRACE_EMBED_BATCH_TIMEOUT env override (default 60s)",
            "decide() returns DeepenAmbiguous on a small RRF top-2 score-gap",
        ] {
            assert!(languageness(s) >= 0.5, "{s} -> {}", languageness(s));
            assert!(!is_garbage(s), "real decision flagged garbage: {s}");
        }
    }

    #[test]
    fn blobs_and_hashes_are_garbage() {
        for s in [
            "a3f9b2c1d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkw",
            "{\"k\":1,\"v\":[2,3],\"q\":null,\"z\":{\"a\":true}}",
            "==>>||::##@@!!&&%%^^$$((**))++",
        ] {
            assert!(is_garbage(s), "garbage not detected: {s} -> {}", languageness(s));
        }
    }

    #[test]
    fn deterministic() {
        let s = "use Postgres instead of MySQL";
        assert_eq!(languageness(s), languageness(s));
    }
}
