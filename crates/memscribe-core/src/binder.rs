//! The binder: decision ↔ edit, with PROV (whitepaper §3, §8.8).
//!
//! For each episode, the binder finds the most recent preceding decision in the
//! same session whose timestamp is `≤` the episode timestamp, and writes a
//! [`BindingEdge`] with a [`ProvRecord`] that satisfies the temporal invariant
//! `t_use ≤ t_gen`. A binding recorded from the deterministic stream is
//! [`FactStatus::DeterministicallyDerived`]. An episode with no preceding
//! decision produces no spurious binding.

use crate::node::{BindingEdge, CorrelationTuple, FactStatus, NodeId, ProvRecord, Relation};
use crate::segmenter::Segmentation;
use std::collections::HashMap;
use std::path::PathBuf;

/// Minimal-support threshold for emitting a [`CorrelationTuple`] (§6/§8.8,
/// "the correct correlation tuple ... when computable"). A decision must bind to
/// **at least this many episodes in the batch** before a contingency table over
/// it carries enough mass to be meaningful; below it, `correlation` is left
/// `None`. A single degenerate arc (one decision → one episode) is therefore
/// never assigned a correlation.
const MIN_CORRELATION_SUPPORT: usize = 2;

/// Decimal places every correlation field is rounded to, so the tuple is a
/// byte-stable function of the integer contingency counts (no float-formatting
/// drift across runs).
const CORRELATION_DECIMALS: i32 = 6;

/// The binder stage.
pub trait Binder {
    /// Produce binding edges from a segmentation.
    fn bind(&self, seg: &Segmentation) -> Vec<BindingEdge>;
}

/// The default deterministic binder.
#[derive(Debug, Default)]
pub struct DefaultBinder;

impl Binder for DefaultBinder {
    fn bind(&self, seg: &Segmentation) -> Vec<BindingEdge> {
        let mut edges = Vec::new();
        // Parallel to `edges`: the bound decision's id and the episode path, so a
        // second pass can compute the co-occurrence contingency table.
        let mut arcs: Vec<(NodeId, PathBuf)> = Vec::new();

        for ep in &seg.episodes {
            // Find the latest decision in the same session that precedes the
            // episode in time (t_use ≤ t_gen). A decision that was rewound away
            // or compacted out (`superseded_by` set) no longer governs current
            // edits, so it is skipped — the binding falls through to the most
            // recent *non-superseded* preceding decision instead (§8.2).
            let mut best: Option<&crate::segmenter::DecisionCandidate> = None;
            for dec in &seg.decisions {
                if dec.session_id != ep.session_id {
                    continue;
                }
                if dec.record.superseded_by.is_some() {
                    continue;
                }
                if dec.timestamp > ep.timestamp {
                    continue;
                }
                // Most-recent-preceding wins; on a timestamp tie the higher
                // turn_seq (later in the stream) wins, so the result is a
                // deterministic function of the input regardless of iteration
                // order.
                let better = match best {
                    None => true,
                    Some(b) => (dec.timestamp, dec.turn_seq) > (b.timestamp, b.turn_seq),
                };
                if better {
                    best = Some(dec);
                }
            }

            let Some(dec) = best else { continue };

            arcs.push((dec.node_id.clone(), ep.episode.path.clone()));
            edges.push(BindingEdge {
                from: dec.node_id.clone(),
                to: ep.node_id.clone(),
                relation: Relation::Produced,
                prov: ProvRecord {
                    used_session: dec.session_id.clone(),
                    used_decision: Some(dec.node_id.clone()),
                    was_generated_by_session: ep.session_id.clone(),
                    t_use: dec.timestamp,
                    t_gen: ep.timestamp,
                },
                fact_status: FactStatus::DeterministicallyDerived,
                correlation: None,
            });
        }

        // Second pass: compute each edge's correlation tuple from the batch-wide
        // co-occurrence contingency table (§6/§8.8). `correlation` stays `None`
        // below the minimal-support threshold ("when computable").
        attach_correlations(&mut edges, &arcs);

        // Deterministic ordering, independent of episode/decision discovery order.
        edges.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.to.cmp(&b.to)));
        edges
    }
}

/// Attach a [`CorrelationTuple`] to every edge whose decision meets the
/// minimal-support threshold, computed from the batch-wide co-occurrence
/// contingency table.
///
/// The "population" is the set of bound arcs (one per binding edge). For a given
/// edge `decision D → episode with path P`, the 2×2 table over that population
/// is, for each arc:
/// - row: is the arc's decision == D?
/// - col: is the arc's path == P?
///
/// yielding counts `n11` (D & P), `n10` (D & ¬P), `n01` (¬D & P),
/// `n00` (¬D & ¬P), with `N = n11+n10+n01+n00`. From these:
/// - `support    = n11 / N`
/// - `confidence = n11 / (n11 + n10)` — `P(P | D)`
/// - `lift       = confidence / P(P)` where `P(P) = (n11 + n01) / N`
/// - `phi        = (n11·n00 − n10·n01) / sqrt(row·row·col·col marginals)`
/// - `p          = erfc(sqrt(chi² / 2))`, `chi² = N · phi²` (1 dof)
///
/// All fields are rounded to [`CORRELATION_DECIMALS`] so the tuple is a stable
/// function of the integer counts. Degenerate tables (a zero marginal makes phi
/// undefined; `lift` undefined when `P(P)=0`) collapse to neutral, finite values
/// (`phi = 0`, `p = 1`, `lift = 0`) rather than NaN — still deterministic.
fn attach_correlations(edges: &mut [BindingEdge], arcs: &[(NodeId, PathBuf)]) {
    // How many arcs each decision binds, so we can apply the support threshold.
    let mut bound_by_decision: HashMap<&NodeId, usize> = HashMap::new();
    for (dec, _) in arcs {
        *bound_by_decision.entry(dec).or_insert(0) += 1;
    }

    let total = arcs.len();
    if total == 0 {
        return;
    }

    for (edge, (dec, path)) in edges.iter_mut().zip(arcs.iter()) {
        // "when computable": require enough co-occurring episodes for the
        // decision before the table carries meaning.
        if bound_by_decision.get(dec).copied().unwrap_or(0) < MIN_CORRELATION_SUPPORT {
            continue;
        }

        let mut n11 = 0u64; // D & P
        let mut n10 = 0u64; // D & ¬P
        let mut n01 = 0u64; // ¬D & P
        let mut n00 = 0u64; // ¬D & ¬P
        for (d, p) in arcs {
            let is_d = d == dec;
            let is_p = p == path;
            match (is_d, is_p) {
                (true, true) => n11 += 1,
                (true, false) => n10 += 1,
                (false, true) => n01 += 1,
                (false, false) => n00 += 1,
            }
        }

        edge.correlation = Some(contingency_to_correlation(n11, n10, n01, n00, total as u64));
    }
}

/// Turn an integer 2×2 contingency table into a rounded [`CorrelationTuple`].
/// Pure and deterministic.
fn contingency_to_correlation(n11: u64, n10: u64, n01: u64, n00: u64, n: u64) -> CorrelationTuple {
    let f = |x: u64| x as f64;
    let n_f = f(n).max(1.0);

    let support = f(n11) / n_f;

    let antecedent = n11 + n10; // arcs with this decision
    let confidence = if antecedent == 0 {
        0.0
    } else {
        f(n11) / f(antecedent)
    };

    let p_consequent = f(n11 + n01) / n_f; // P(path = P)
    let lift = if p_consequent == 0.0 {
        0.0
    } else {
        confidence / p_consequent
    };

    // phi = (n11 n00 − n10 n01) / sqrt(product of the four marginals).
    let row1 = f(n11 + n10);
    let row0 = f(n01 + n00);
    let col1 = f(n11 + n01);
    let col0 = f(n10 + n00);
    let denom = (row1 * row0 * col1 * col0).sqrt();
    let phi = if denom == 0.0 {
        0.0
    } else {
        (f(n11) * f(n00) - f(n10) * f(n01)) / denom
    };

    // chi-square (1 dof) = N · phi²; two-sided p-value = erfc(sqrt(chi²/2)).
    let chi2 = n_f * phi * phi;
    let p = erfc((chi2 / 2.0).sqrt());

    CorrelationTuple {
        support: round(support),
        confidence: round(confidence),
        lift: round(lift),
        phi: round(phi),
        p: round(p),
    }
}

/// Round to [`CORRELATION_DECIMALS`] places — stable across runs.
fn round(x: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let scale = 10f64.powi(CORRELATION_DECIMALS);
    (x * scale).round() / scale
}

/// Complementary error function — the Numerical-Recipes `erfcc` rational
/// approximation (fractional error < 1.2e-7 everywhere). Uses only `exp` and
/// arithmetic, so it is a deterministic function of its input on a given target.
fn erfc(x: f64) -> f64 {
    let z = x.abs();
    let t = 1.0 / (1.0 + 0.5 * z);
    let ans = t
        * (-z * z - 1.265_512_23
            + t * (1.000_023_68
                + t * (0.374_091_96
                    + t * (0.096_784_18
                        + t * (-0.186_288_06
                            + t * (0.278_868_07
                                + t * (-1.135_203_98
                                    + t * (1.488_515_87
                                        + t * (-0.822_152_23 + t * 0.170_872_77)))))))))
            .exp();
    if x >= 0.0 {
        ans
    } else {
        2.0 - ans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Diff;
    use crate::node::{CodeEpisode, DecisionRecord, FactStatus as FS};
    use crate::segmenter::{DecisionCandidate, EpisodeRecord, Segmentation};
    use time::OffsetDateTime;

    fn ts(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
    }

    fn decision(id: &str, session: &str, turn_seq: u64, t: i64) -> DecisionCandidate {
        DecisionCandidate {
            record: DecisionRecord {
                epitome: format!("decision {id}"),
                considered_options: vec![],
                is_ban: false,
                superseded_by: None,
                confirmation: None,
                source_span: turn_seq..turn_seq + 1,
                fact_status: FS::Observed,
            },
            node_id: NodeId::new(id),
            turn_seq,
            timestamp: ts(t),
            session_id: session.to_string(),
        }
    }

    fn superseded(mut d: DecisionCandidate, marker: &str) -> DecisionCandidate {
        d.record.superseded_by = Some(NodeId::new(marker));
        d
    }

    fn episode(id: &str, session: &str, seq: u64, t: i64, path: &str) -> EpisodeRecord {
        EpisodeRecord {
            episode: CodeEpisode {
                path: PathBuf::from(path),
                diff: Diff::for_path(path),
                git: None,
                episode_id: id.to_string(),
            },
            node_id: NodeId::new(format!("episode:{id}")),
            seq,
            timestamp: ts(t),
            session_id: session.to_string(),
        }
    }

    // ---- Task C: structural scenarios that must still hold ----

    #[test]
    fn interleaved_arcs_bind_to_most_recent_preceding_decision() {
        // Two decisions in one session; an edit after each binds to the nearest
        // preceding decision, not the older one.
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![
                decision("dec:A", "s1", 1, 10),
                decision("dec:B", "s1", 3, 30),
            ],
            episodes: vec![
                episode("e1", "s1", 2, 20, "a.rs"), // after A, before B → A
                episode("e2", "s1", 4, 40, "b.rs"), // after B → B
            ],
        };
        let edges = DefaultBinder.bind(&seg);
        let a = edges.iter().find(|e| e.to.0 == "episode:e1").unwrap();
        let b = edges.iter().find(|e| e.to.0 == "episode:e2").unwrap();
        assert_eq!(a.from.0, "dec:A");
        assert_eq!(b.from.0, "dec:B");
    }

    #[test]
    fn multi_edit_one_decision_n_episodes_n_bindings() {
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![decision("dec:A", "s1", 1, 10)],
            episodes: vec![
                episode("e1", "s1", 2, 20, "a.rs"),
                episode("e2", "s1", 3, 30, "b.rs"),
                episode("e3", "s1", 4, 40, "c.rs"),
            ],
        };
        let edges = DefaultBinder.bind(&seg);
        assert_eq!(edges.len(), 3);
        assert!(edges.iter().all(|e| e.from.0 == "dec:A"));
    }

    #[test]
    fn no_decision_no_spurious_binding() {
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![],
            episodes: vec![episode("e1", "s1", 2, 20, "a.rs")],
        };
        assert!(DefaultBinder.bind(&seg).is_empty());
    }

    // ---- Task A (binder side): superseded decisions don't bind ----

    #[test]
    fn superseded_decision_is_skipped_and_falls_through() {
        // Decision A is rewound away; decision B governs. The edit after B binds
        // to B, never to the superseded A.
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![
                superseded(decision("dec:A", "s1", 1, 10), "rewind:s1:2"),
                decision("dec:B", "s1", 5, 50),
            ],
            episodes: vec![episode("e1", "s1", 6, 60, "a.rs")],
        };
        let edges = DefaultBinder.bind(&seg);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].from.0, "dec:B");
    }

    #[test]
    fn only_superseded_decision_yields_no_binding() {
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![superseded(decision("dec:A", "s1", 1, 10), "rewind:s1:2")],
            episodes: vec![episode("e1", "s1", 6, 60, "a.rs")],
        };
        assert!(DefaultBinder.bind(&seg).is_empty());
    }

    // ---- Task B: correlation tuple ----

    #[test]
    fn repeated_cooccurrence_yields_some_correlation_with_sane_values() {
        // One decision binds three episodes (>= MIN_CORRELATION_SUPPORT), two of
        // which touch the same path. Correlation must be Some and in-range.
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![decision("dec:A", "s1", 1, 10)],
            episodes: vec![
                episode("e1", "s1", 2, 20, "auth.rs"),
                episode("e2", "s1", 3, 30, "auth.rs"),
                episode("e3", "s1", 4, 40, "db.rs"),
            ],
        };
        let edges = DefaultBinder.bind(&seg);
        assert_eq!(edges.len(), 3);
        for e in &edges {
            let c = e.correlation.as_ref().expect("correlation present");
            assert!((0.0..=1.0).contains(&c.support), "support {}", c.support);
            assert!(
                (0.0..=1.0).contains(&c.confidence),
                "confidence {}",
                c.confidence
            );
            assert!(c.lift >= 0.0, "lift {}", c.lift);
            assert!((-1.0..=1.0).contains(&c.phi), "phi {}", c.phi);
            assert!((0.0..=1.0).contains(&c.p), "p {}", c.p);
        }
        // fact_status stays DeterministicallyDerived even with correlation set.
        assert!(edges
            .iter()
            .all(|e| e.fact_status == FactStatus::DeterministicallyDerived));
    }

    #[test]
    fn single_degenerate_arc_yields_none() {
        // A lone decision→episode arc has support 1 < threshold → no correlation.
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![decision("dec:A", "s1", 1, 10)],
            episodes: vec![episode("e1", "s1", 2, 20, "a.rs")],
        };
        let edges = DefaultBinder.bind(&seg);
        assert_eq!(edges.len(), 1);
        assert!(edges[0].correlation.is_none());
    }

    #[test]
    fn correlation_is_deterministic_across_runs() {
        let seg = Segmentation {
            conversations: vec![],
            decisions: vec![
                decision("dec:A", "s1", 1, 10),
                decision("dec:B", "s1", 5, 50),
            ],
            episodes: vec![
                episode("e1", "s1", 2, 20, "auth.rs"),
                episode("e2", "s1", 3, 30, "auth.rs"),
                episode("e3", "s1", 4, 40, "db.rs"),
                episode("e4", "s1", 6, 60, "db.rs"),
                episode("e5", "s1", 7, 70, "auth.rs"),
            ],
        };
        let a = DefaultBinder.bind(&seg);
        let b = DefaultBinder.bind(&seg);
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb);
    }

    #[test]
    fn contingency_table_matches_hand_computed_values() {
        // n11=2, n10=1, n01=1, n00=2, N=6. Standard market-basket arithmetic.
        let c = contingency_to_correlation(2, 1, 1, 2, 6);
        assert!((c.support - 0.333_333).abs() < 1e-6);
        // confidence = 2/3
        assert!((c.confidence - 0.666_667).abs() < 1e-6);
        // P(P) = 3/6 = 0.5 → lift = (2/3)/0.5 = 1.333333
        assert!((c.lift - 1.333_333).abs() < 1e-6);
        // phi = (2*2 - 1*1)/sqrt(3*3*3*3) = 3/9 = 0.333333
        assert!((c.phi - 0.333_333).abs() < 1e-6);
        assert!((0.0..=1.0).contains(&c.p));
    }

    #[test]
    fn erfc_is_sane_at_known_points() {
        assert!((erfc(0.0) - 1.0).abs() < 1e-6);
        assert!(erfc(5.0) < 1e-6);
        assert!((erfc(-5.0) - 2.0).abs() < 1e-6);
    }
}
