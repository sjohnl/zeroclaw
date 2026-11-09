//! Per-model monthly spend caps, read from ZeroClaw's own cost ledger.
//!
//! ZeroClaw enforces daily and monthly totals and per-agent daily ceilings,
//! but not a monthly cap per model. This guard adds that, without a counter
//! of its own: before each model call it re-reads the daemon's append-only
//! `costs.jsonl`, sums the model's month-to-date `cost_usd`, and refuses the
//! call unless the remaining headroom covers one more call.
//!
//! The guard fails closed. A missing or damaged ledger, or any usage of the
//! model this month that the daemon could not price, denies the call,
//! because recorded spend is then only a lower bound.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use zeroclaw_config::cost::CostRecord;

/// One model's spend in the UTC calendar month containing `now`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelMonthSpend {
    pub model: String,
    pub cost_usd: f64,
    /// Tokens the daemon recorded without a price. Non-zero means
    /// `cost_usd` understates real spend.
    pub unpriced_tokens: u64,
    pub requests: usize,
}

/// Strip any `provider/` routing prefixes, mirroring how the daemon matches
/// `anthropic/claude-x` against a rate stored as `claude-x`.
fn model_base(model: &str) -> &str {
    model.rsplit_once('/').map_or(model, |(_, base)| base)
}

/// Whether a ledger model id refers to the configured model.
#[must_use]
pub fn model_matches(recorded: &str, configured: &str) -> bool {
    recorded == configured || model_base(recorded) == model_base(configured)
}

fn same_utc_month(a: DateTime<Utc>, b: DateTime<Utc>) -> bool {
    a.year() == b.year() && a.month() == b.month()
}

/// Sum `model`'s spend for the month containing `now`.
///
/// Lines are parsed as the daemon's own [`CostRecord`] type, including the
/// daemon's tolerance for two records concatenated on one line by
/// concurrent appends. A malformed final line is skipped as an append in
/// progress; a malformed line anywhere else is an error.
pub fn month_spend(ledger: &Path, model: &str, now: DateTime<Utc>) -> Result<ModelMonthSpend> {
    let text = fs::read_to_string(ledger).with_context(|| {
        format!(
            "reading cost ledger {}; the harness refuses to spend without it. On a \
             daemon that has not recorded any cost yet, create it empty",
            ledger.display()
        )
    })?;
    let lines: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| !l.is_empty())
        .collect();
    let last_line = lines.last().map(|(n, _)| *n);

    let mut spend = ModelMonthSpend {
        model: model.to_owned(),
        cost_usd: 0.0,
        unpriced_tokens: 0,
        requests: 0,
    };
    for (line_no, line) in lines {
        let records: Result<Vec<CostRecord>, _> = serde_json::Deserializer::from_str(line)
            .into_iter::<CostRecord>()
            .collect();
        let records = match records {
            Ok(records) => records,
            Err(_) if Some(line_no) == last_line => continue,
            Err(err) => bail!(
                "cost ledger {} line {line_no} is malformed ({err}); spend cannot be \
                 trusted until it is repaired",
                ledger.display()
            ),
        };
        for record in records {
            let usage = &record.usage;
            if !model_matches(&usage.model, model) || !same_utc_month(usage.timestamp, now) {
                continue;
            }
            spend.requests += 1;
            if usage.cost_usd.is_finite() && usage.cost_usd > 0.0 {
                spend.cost_usd += usage.cost_usd;
            }
            spend.unpriced_tokens += usage.unpriced_tokens;
            if !usage.pricing_available && usage.unpriced_tokens == 0 {
                // Rows written before dimension-level provenance carry only
                // the summary flag; count every token as unpriced.
                spend.unpriced_tokens += usage.total_tokens.max(1);
            }
        }
    }
    Ok(spend)
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum BudgetDecision {
    Allowed {
        spend: ModelMonthSpend,
        remaining_usd: f64,
    },
    Denied {
        model: String,
        reason: String,
    },
}

impl BudgetDecision {
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed { .. })
    }
}

/// The per-model cap check run before every model call.
#[derive(Debug, Clone)]
pub struct BudgetGuard {
    ledger: PathBuf,
    monthly_cap_usd: f64,
    reserve_usd: f64,
}

impl BudgetGuard {
    #[must_use]
    pub fn new(ledger: PathBuf, monthly_cap_usd: f64, reserve_usd: f64) -> Self {
        Self {
            ledger,
            monthly_cap_usd,
            reserve_usd,
        }
    }

    #[must_use]
    pub fn monthly_cap_usd(&self) -> f64 {
        self.monthly_cap_usd
    }

    /// Month-to-date spend for `model`.
    pub fn spend(&self, model: &str, now: DateTime<Utc>) -> Result<ModelMonthSpend> {
        month_spend(&self.ledger, model, now)
    }

    /// May one more call to `model` be made now?
    #[must_use]
    pub fn check(&self, model: &str, now: DateTime<Utc>) -> BudgetDecision {
        let spend = match month_spend(&self.ledger, model, now) {
            Ok(spend) => spend,
            Err(err) => {
                return BudgetDecision::Denied {
                    model: model.to_owned(),
                    reason: format!("{err:#}"),
                };
            }
        };
        if spend.unpriced_tokens > 0 {
            return BudgetDecision::Denied {
                model: model.to_owned(),
                reason: format!(
                    "{} tokens of {model} usage this month have no price, so its spend is \
                     unknown; add the missing [cost.rates] entries for this model",
                    spend.unpriced_tokens
                ),
            };
        }
        let remaining = self.monthly_cap_usd - spend.cost_usd;
        if remaining < self.reserve_usd {
            return BudgetDecision::Denied {
                model: model.to_owned(),
                reason: format!(
                    "{model} has spent ${:.4} of its ${:.2} monthly cap; one more call \
                     needs ${:.2} of headroom",
                    spend.cost_usd, self.monthly_cap_usd, self.reserve_usd
                ),
            };
        }
        BudgetDecision::Allowed {
            spend,
            remaining_usd: remaining,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Write;
    use zeroclaw_config::cost::TokenUsage;

    fn record(model: &str, cost: f64, at: DateTime<Utc>) -> CostRecord {
        let mut usage = TokenUsage::new(model, 1_000, 200, 0, 0.0, 0.0, 0.0);
        usage.cost_usd = cost;
        usage.timestamp = at;
        CostRecord::with_agent("daemon-1", Some("brief_writer".into()), usage)
    }

    fn ledger_with(records: &[CostRecord]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for r in records {
            writeln!(file, "{}", serde_json::to_string(r).unwrap()).unwrap();
        }
        file
    }

    fn nov(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
    }

    #[test]
    fn sums_only_this_model_this_month() {
        let october = Utc.with_ymd_and_hms(2026, 10, 31, 23, 59, 0).unwrap();
        let file = ledger_with(&[
            record("anthropic/claude-sonnet-4-5", 3.0, nov(2)),
            record("claude-sonnet-4-5", 1.5, nov(5)),
            record("claude-sonnet-4-5", 9.0, october),
            record("gpt-5-mini", 4.0, nov(5)),
        ]);
        let spend = month_spend(file.path(), "claude-sonnet-4-5", nov(8)).unwrap();
        assert!((spend.cost_usd - 4.5).abs() < 1e-9, "{spend:?}");
        assert_eq!(spend.requests, 2);
    }

    #[test]
    fn allows_with_headroom_and_denies_at_cap() {
        let file = ledger_with(&[record("claude-haiku-4-5", 19.70, nov(3))]);
        let guard = BudgetGuard::new(file.path().to_path_buf(), 20.0, 0.25);
        let BudgetDecision::Allowed { remaining_usd, .. } = guard.check("claude-haiku-4-5", nov(8))
        else {
            panic!("0.30 of headroom covers a 0.25 reserve");
        };
        assert!((remaining_usd - 0.30).abs() < 1e-9);

        let tight = BudgetGuard::new(file.path().to_path_buf(), 20.0, 0.50);
        assert!(!tight.check("claude-haiku-4-5", nov(8)).is_allowed());
        assert!(
            tight.check("gpt-5-mini", nov(8)).is_allowed(),
            "caps are per model"
        );
    }

    #[test]
    fn new_month_resets_the_cap() {
        let file = ledger_with(&[record("claude-haiku-4-5", 25.0, nov(30))]);
        let guard = BudgetGuard::new(file.path().to_path_buf(), 20.0, 0.25);
        assert!(!guard.check("claude-haiku-4-5", nov(30)).is_allowed());
        let december = Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 1).unwrap();
        assert!(guard.check("claude-haiku-4-5", december).is_allowed());
    }

    #[test]
    fn unpriced_usage_denies() {
        let mut r = record("gpt-5-mini", 0.10, nov(4));
        r.usage.unpriced_tokens = 500;
        r.usage.pricing_available = false;
        let file = ledger_with(&[r]);
        let guard = BudgetGuard::new(file.path().to_path_buf(), 20.0, 0.25);
        let BudgetDecision::Denied { reason, .. } = guard.check("gpt-5-mini", nov(8)) else {
            panic!("unknown spend must deny");
        };
        assert!(reason.contains("no price"), "{reason}");
    }

    #[test]
    fn missing_ledger_denies() {
        let dir = tempfile::tempdir().unwrap();
        let guard = BudgetGuard::new(dir.path().join("costs.jsonl"), 20.0, 0.25);
        assert!(!guard.check("gpt-5-mini", nov(8)).is_allowed());
    }

    #[test]
    fn malformed_interior_line_denies_but_partial_tail_is_tolerated() {
        let good = serde_json::to_string(&record("gpt-5-mini", 1.0, nov(2))).unwrap();
        let mut tail = tempfile::NamedTempFile::new().unwrap();
        write!(tail, "{good}\n{{\"id\":\"half-writ").unwrap();
        assert!(month_spend(tail.path(), "gpt-5-mini", nov(8)).is_ok());

        let mut interior = tempfile::NamedTempFile::new().unwrap();
        write!(interior, "{{broken\n{good}\n").unwrap();
        assert!(month_spend(interior.path(), "gpt-5-mini", nov(8)).is_err());
    }

    #[test]
    fn concatenated_records_on_one_line_are_both_counted() {
        let a = serde_json::to_string(&record("gpt-5-mini", 1.0, nov(2))).unwrap();
        let b = serde_json::to_string(&record("gpt-5-mini", 2.0, nov(3))).unwrap();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "{a}{b}").unwrap();
        let spend = month_spend(file.path(), "gpt-5-mini", nov(8)).unwrap();
        assert!((spend.cost_usd - 3.0).abs() < 1e-9);
        assert_eq!(spend.requests, 2);
    }

    #[test]
    fn model_matching_ignores_routing_prefixes() {
        assert!(model_matches("openrouter/anthropic/claude-x", "claude-x"));
        assert!(model_matches("claude-x", "anthropic/claude-x"));
        assert!(!model_matches("claude-x-mini", "claude-x"));
    }
}
