//! Structured model outputs and their strict parsers.
//!
//! Every model reply must be one JSON object matching a fixed shape. Unknown
//! fields, missing fields, and out-of-range indices are errors, never
//! guesses: a reply the harness cannot parse exactly is treated as a failed
//! step.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The writer's output: the brief a subscriber receives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Brief {
    pub headline: String,
    pub items: Vec<BriefItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BriefItem {
    pub claim: String,
    pub why_it_matters: String,
    pub citations: Vec<Citation>,
}

/// Evidence for a claim: a verbatim quote from one stored snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Citation {
    pub source_id: String,
    /// SHA-256 of the normalized snapshot text the quote comes from.
    pub snapshot: String,
    pub quote: String,
}

/// The analyst's output: which numbered changes deserve a brief item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Triage {
    pub relevant: Vec<TriageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TriageItem {
    /// 1-based change number from the analyst prompt.
    pub change: usize,
    pub reason: String,
}

/// The verifier's output: one verdict per brief item.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SemanticReview {
    pub verdicts: Vec<SemanticVerdict>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SemanticVerdict {
    /// 1-based brief item number.
    pub item: usize,
    pub supported: bool,
    pub reason: String,
}

/// Return the single JSON object in a model reply, tolerating a Markdown
/// code fence or a sentence of prose around it. Returns `None` when there is
/// no `{ ... }` span.
#[must_use]
pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    (end > start).then(|| &text[start..=end])
}

fn parse_object<T: for<'de> Deserialize<'de>>(text: &str, what: &str) -> Result<T> {
    let Some(json) = extract_json_object(text) else {
        bail!("{what} reply contains no JSON object");
    };
    serde_json::from_str(json).with_context(|| format!("{what} reply does not match its schema"))
}

/// Parse the writer's brief. Shape only; evidence is checked by
/// [`crate::verify`].
pub fn parse_brief(text: &str, max_items: usize) -> Result<Brief> {
    let brief: Brief = parse_object(text, "writer")?;
    if brief.headline.trim().is_empty() {
        bail!("writer brief has an empty headline");
    }
    if brief.items.is_empty() {
        bail!("writer brief has no items");
    }
    if brief.items.len() > max_items {
        bail!(
            "writer brief has {} items; the pack allows at most {max_items}",
            brief.items.len()
        );
    }
    Ok(brief)
}

/// Parse the analyst's triage. Change numbers must be within
/// `1..=change_count` and appear once.
pub fn parse_triage(text: &str, change_count: usize) -> Result<Triage> {
    let triage: Triage = parse_object(text, "analyst")?;
    let mut seen = BTreeSet::new();
    for item in &triage.relevant {
        if item.change == 0 || item.change > change_count {
            bail!(
                "analyst named change {}, but only 1..={change_count} exist",
                item.change
            );
        }
        if !seen.insert(item.change) {
            bail!("analyst named change {} twice", item.change);
        }
    }
    Ok(triage)
}

/// Parse the verifier's review. It must hold exactly one verdict for each
/// item `1..=item_count`.
pub fn parse_semantic_review(text: &str, item_count: usize) -> Result<SemanticReview> {
    let review: SemanticReview = parse_object(text, "verifier")?;
    let items: BTreeSet<usize> = review.verdicts.iter().map(|v| v.item).collect();
    let expected: BTreeSet<usize> = (1..=item_count).collect();
    if items != expected || review.verdicts.len() != item_count {
        bail!("verifier must return exactly one verdict for each item 1..={item_count}");
    }
    Ok(review)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIEF: &str = r#"{"headline":"Two new bids","items":[{"claim":"City opened a network bid","why_it_matters":"Fits your services","citations":[{"source_id":"city-bids","snapshot":"abc","quote":"Network refresh RFP"}]}]}"#;

    #[test]
    fn extracts_object_from_fence_and_prose() {
        let wrapped = format!("Here you go:\n```json\n{BRIEF}\n```\nThanks");
        assert_eq!(extract_json_object(&wrapped), Some(BRIEF));
        assert_eq!(extract_json_object("no json here"), None);
        assert_eq!(extract_json_object("} backwards {"), None);
    }

    #[test]
    fn parses_valid_brief() {
        let brief = parse_brief(BRIEF, 5).unwrap();
        assert_eq!(brief.items.len(), 1);
        assert_eq!(brief.items[0].citations[0].source_id, "city-bids");
    }

    #[test]
    fn brief_rejects_unknown_fields_and_bad_counts() {
        let extra = BRIEF.replace(r#""headline""#, r#""send_now":true,"headline""#);
        assert!(parse_brief(&extra, 5).is_err());
        assert!(parse_brief(BRIEF, 0).is_err(), "over the item limit");
        assert!(parse_brief(r#"{"headline":"x","items":[]}"#, 5).is_err());
        assert!(parse_brief(r#"{"headline":" ","items":[]}"#, 5).is_err());
    }

    #[test]
    fn triage_indices_are_bounded_and_unique() {
        let ok = r#"{"relevant":[{"change":1,"reason":"a"},{"change":3,"reason":"b"}]}"#;
        assert_eq!(parse_triage(ok, 3).unwrap().relevant.len(), 2);
        assert!(parse_triage(ok, 2).is_err(), "change 3 out of range");
        assert!(parse_triage(r#"{"relevant":[{"change":0,"reason":"a"}]}"#, 3).is_err());
        let dup = r#"{"relevant":[{"change":1,"reason":"a"},{"change":1,"reason":"b"}]}"#;
        assert!(parse_triage(dup, 3).is_err());
        assert!(
            parse_triage(r#"{"relevant":[]}"#, 3)
                .unwrap()
                .relevant
                .is_empty()
        );
    }

    #[test]
    fn review_needs_exactly_one_verdict_per_item() {
        let ok = r#"{"verdicts":[{"item":1,"supported":true,"reason":"ok"},{"item":2,"supported":false,"reason":"no"}]}"#;
        assert_eq!(parse_semantic_review(ok, 2).unwrap().verdicts.len(), 2);
        assert!(parse_semantic_review(ok, 3).is_err(), "missing item 3");
        let dup = r#"{"verdicts":[{"item":1,"supported":true,"reason":"ok"},{"item":1,"supported":true,"reason":"ok"}]}"#;
        assert!(parse_semantic_review(dup, 2).is_err());
    }
}
