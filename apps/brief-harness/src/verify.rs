//! Deterministic citation gate: the first and hardest check on a brief.
//!
//! A brief item passes only when it cites a snapshot captured in this run,
//! names that snapshot's source, quotes it verbatim, and at least one quote
//! comes from text that actually changed. No model is consulted; the
//! semantic review in the pipeline runs only after this gate passes.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use crate::brief::Brief;
use crate::snapshot::{Change, SnapshotStore, collapse_whitespace};

/// Shortest quote accepted, in characters after whitespace collapsing.
/// Shorter strings ("the", "RFP") match almost anywhere and prove nothing.
pub const MIN_QUOTE_CHARS: usize = 12;

/// One snapshot the writer was shown, with the lines that changed in it.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub source_id: String,
    pub sha256: String,
    /// Whole snapshot text, whitespace-collapsed into one line.
    flat_text: String,
    /// Changed lines, whitespace-collapsed.
    changed_lines: Vec<String>,
}

impl Evidence {
    #[must_use]
    pub fn new(source_id: &str, sha256: &str, text: &str, changed_lines: &[String]) -> Self {
        Self {
            source_id: source_id.to_owned(),
            sha256: sha256.to_owned(),
            flat_text: collapse_whitespace(text),
            changed_lines: changed_lines
                .iter()
                .map(|l| collapse_whitespace(l))
                .filter(|l| !l.is_empty())
                .collect(),
        }
    }
}

/// The snapshots a run may cite, keyed by hash.
#[derive(Debug, Clone, Default)]
pub struct EvidenceSet {
    by_sha: HashMap<String, Evidence>,
}

impl EvidenceSet {
    pub fn insert(&mut self, evidence: Evidence) {
        self.by_sha.insert(evidence.sha256.clone(), evidence);
    }

    /// Evidence for the changes the writer was shown: each change's current
    /// snapshot (anchored on added lines) and previous snapshot (anchored on
    /// removed lines), read back through the hash check.
    pub fn from_changes<'a>(
        changes: impl IntoIterator<Item = &'a Change>,
        store: &SnapshotStore,
    ) -> Result<Self> {
        let mut set = Self::default();
        for change in changes {
            let current = store.read_text(&change.current.sha256)?;
            set.insert(Evidence::new(
                &change.current.source_id,
                &change.current.sha256,
                &current,
                &change.added,
            ));
            let previous = store.read_text(&change.previous.sha256)?;
            set.insert(Evidence::new(
                &change.previous.source_id,
                &change.previous.sha256,
                &previous,
                &change.removed,
            ));
        }
        Ok(set)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.by_sha.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_sha.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ItemCheck {
    /// 1-based item number.
    pub item: usize,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VerificationReport {
    pub passed: bool,
    pub items: Vec<ItemCheck>,
}

impl VerificationReport {
    /// Every issue, prefixed with its item number.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        self.items
            .iter()
            .flat_map(|c| {
                c.issues
                    .iter()
                    .map(move |i| format!("item {}: {i}", c.item))
            })
            .collect()
    }
}

/// Check every citation in `brief` against `evidence`.
#[must_use]
pub fn verify_brief(brief: &Brief, evidence: &EvidenceSet) -> VerificationReport {
    let items: Vec<ItemCheck> = brief
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mut issues = Vec::new();
            if item.claim.trim().is_empty() {
                issues.push("claim is empty".to_owned());
            }
            if item.citations.is_empty() {
                issues.push("claim has no citations".to_owned());
            }
            let mut anchored = false;
            for (n, citation) in item.citations.iter().enumerate() {
                let n = n + 1;
                let Some(ev) = evidence.by_sha.get(&citation.snapshot) else {
                    issues.push(format!(
                        "citation {n} cites snapshot {:?}, which was not captured in this run",
                        citation.snapshot
                    ));
                    continue;
                };
                if ev.source_id != citation.source_id {
                    issues.push(format!(
                        "citation {n} names source {:?} but the snapshot belongs to {:?}",
                        citation.source_id, ev.source_id
                    ));
                    continue;
                }
                let quote = collapse_whitespace(&citation.quote);
                if quote.chars().count() < MIN_QUOTE_CHARS {
                    issues.push(format!(
                        "citation {n} quote is shorter than {MIN_QUOTE_CHARS} characters"
                    ));
                    continue;
                }
                if !ev.flat_text.contains(&quote) {
                    issues.push(format!(
                        "citation {n} quote does not appear in the snapshot"
                    ));
                    continue;
                }
                if ev
                    .changed_lines
                    .iter()
                    .any(|line| line.contains(&quote) || quote.contains(line.as_str()))
                {
                    anchored = true;
                }
            }
            if !item.citations.is_empty() && !anchored && issues.is_empty() {
                issues.push("no citation quotes text that changed in this run".to_owned());
            }
            ItemCheck {
                item: index + 1,
                issues,
            }
        })
        .collect();
    let passed = !items.is_empty() && items.iter().all(|c| c.issues.is_empty());
    VerificationReport { passed, items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brief::{BriefItem, Citation};

    const CUR: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const PREV: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn evidence() -> EvidenceSet {
        let mut set = EvidenceSet::default();
        set.insert(Evidence::new(
            "city-bids",
            CUR,
            "Open bids\nNetwork   refresh RFP due 2026-12-01\nJanitorial services",
            &["Network refresh RFP due 2026-12-01".to_owned()],
        ));
        set.insert(Evidence::new(
            "city-bids",
            PREV,
            "Open bids\nJanitorial services\nRoof repair closes Friday",
            &["Roof repair closes Friday".to_owned()],
        ));
        set
    }

    fn brief(citations: Vec<Citation>) -> Brief {
        Brief {
            headline: "h".into(),
            items: vec![BriefItem {
                claim: "The city opened a network RFP".into(),
                why_it_matters: "w".into(),
                citations,
            }],
        }
    }

    fn cite(source: &str, snapshot: &str, quote: &str) -> Citation {
        Citation {
            source_id: source.into(),
            snapshot: snapshot.into(),
            quote: quote.into(),
        }
    }

    #[test]
    fn verbatim_quote_of_changed_line_passes() {
        let report = verify_brief(
            &brief(vec![cite(
                "city-bids",
                CUR,
                "Network refresh RFP due 2026-12-01",
            )]),
            &evidence(),
        );
        assert!(report.passed, "{:?}", report.failures());
    }

    #[test]
    fn whitespace_differences_do_not_matter() {
        let report = verify_brief(
            &brief(vec![cite("city-bids", CUR, "Network\n refresh   RFP due")]),
            &evidence(),
        );
        assert!(report.passed, "{:?}", report.failures());
    }

    #[test]
    fn removed_text_is_citable_from_the_previous_snapshot() {
        let report = verify_brief(
            &brief(vec![cite("city-bids", PREV, "Roof repair closes Friday")]),
            &evidence(),
        );
        assert!(report.passed, "{:?}", report.failures());
    }

    #[test]
    fn invented_quote_fails() {
        let report = verify_brief(
            &brief(vec![cite(
                "city-bids",
                CUR,
                "Network refresh RFP worth $2M",
            )]),
            &evidence(),
        );
        assert!(!report.passed);
        assert!(report.failures()[0].contains("does not appear"));
    }

    #[test]
    fn unknown_snapshot_and_wrong_source_fail() {
        let other = "c".repeat(64);
        let r1 = verify_brief(
            &brief(vec![cite("city-bids", &other, "Network refresh RFP")]),
            &evidence(),
        );
        assert!(!r1.passed);
        let r2 = verify_brief(
            &brief(vec![cite("state-grants", CUR, "Network refresh RFP")]),
            &evidence(),
        );
        assert!(!r2.passed);
    }

    #[test]
    fn unchanged_text_alone_is_not_enough() {
        let report = verify_brief(
            &brief(vec![cite("city-bids", CUR, "Janitorial services")]),
            &evidence(),
        );
        assert!(!report.passed);
        assert!(report.failures()[0].contains("changed"));
    }

    #[test]
    fn short_quote_and_missing_citations_fail() {
        assert!(!verify_brief(&brief(vec![cite("city-bids", CUR, "RFP")]), &evidence()).passed);
        assert!(!verify_brief(&brief(vec![]), &evidence()).passed);
    }

    #[test]
    fn one_bad_item_fails_the_brief() {
        let mut b = brief(vec![cite("city-bids", CUR, "Network refresh RFP due")]);
        b.items.push(BriefItem {
            claim: "Made up".into(),
            why_it_matters: "w".into(),
            citations: vec![cite("city-bids", CUR, "This text is not there at all")],
        });
        let report = verify_brief(&b, &evidence());
        assert!(!report.passed);
        assert!(report.items[0].issues.is_empty());
        assert_eq!(report.items[1].item, 2);
    }
}
