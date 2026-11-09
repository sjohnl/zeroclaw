//! One subscription run, end to end, plus approval resolution.
//!
//! Operational outcomes (no changes, a denied budget, a failed agent call,
//! a held brief) are reported as a [`RunStatus`] and audited; they are not
//! errors. `Err` is reserved for faults in the harness's own state, such as
//! an unwritable state directory or audit log, because a run that cannot
//! record what it did must stop.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;

use crate::a2a::AgentClient;
use crate::approvals::{ApprovalQueue, Resolution};
use crate::audit::{AuditEvent, AuditLog};
use crate::brief::{parse_brief, parse_semantic_review, parse_triage};
use crate::budget::{BudgetDecision, BudgetGuard};
use crate::config::{HarnessConfig, Role, Subscription};
use crate::delivery::{
    Deliverer, DeliveryDecision, DeliveryEnvelope, GateResults, SourceRef, decide, render_markdown,
};
use crate::fsutil::{sha256_hex, write_json_atomic};
use crate::pack::{SourceSpec, VerticalPack};
use crate::prompts::{analyst_prompt, render_changes, verifier_prompt, writer_prompt};
use crate::snapshot::{Change, ChangeOutcome, Fetcher, SnapshotStore, normalize};
use crate::verify::{EvidenceSet, verify_brief};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    KillSwitch,
    NoChanges,
    NothingRelevant,
    BudgetDenied,
    AgentFailed,
    BriefRejected,
    Held,
    QueuedForApproval,
    Delivered,
    DeliveryFailed,
}

impl RunStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::KillSwitch => "kill_switch",
            Self::NoChanges => "no_changes",
            Self::NothingRelevant => "nothing_relevant",
            Self::BudgetDenied => "budget_denied",
            Self::AgentFailed => "agent_failed",
            Self::BriefRejected => "brief_rejected",
            Self::Held => "held",
            Self::QueuedForApproval => "queued_for_approval",
            Self::Delivered => "delivered",
            Self::DeliveryFailed => "delivery_failed",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub subscription: String,
    pub status: RunStatus,
    pub changes: usize,
    pub detail: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_path: Option<String>,
}

/// Why a model step did not produce a reply.
struct Stop {
    status: RunStatus,
    detail: String,
}

#[derive(Serialize)]
struct HeldBrief<'a> {
    reasons: &'a [String],
    envelope: &'a DeliveryEnvelope,
}

/// The harness over its collaborators: a fetcher, the agent client, and a
/// deliverer. Production wiring uses [`crate::snapshot::HttpFetcher`],
/// [`crate::a2a::A2aClient`], and [`crate::delivery::LiveDeliverer`].
pub struct Harness<F, A, D> {
    config: HarnessConfig,
    packs: BTreeMap<String, VerticalPack>,
    fetcher: F,
    agents: A,
    deliverer: D,
    budget: BudgetGuard,
    store: SnapshotStore,
    approvals: ApprovalQueue,
}

impl<F: Fetcher, A: AgentClient, D: Deliverer> Harness<F, A, D> {
    pub fn new(
        config: HarnessConfig,
        packs: BTreeMap<String, VerticalPack>,
        fetcher: F,
        agents: A,
        deliverer: D,
    ) -> Result<Self> {
        config.validate_against_packs(&packs)?;
        let budget = BudgetGuard::new(
            config.zeroclaw.cost_ledger.clone(),
            config.budget.monthly_cap_usd_per_model,
            config.budget.reserve_usd_per_call,
        );
        let store = SnapshotStore::new(config.state_dir.join("snapshots"));
        let approvals = ApprovalQueue::new(config.state_dir.join("approvals"));
        Ok(Self {
            config,
            packs,
            fetcher,
            agents,
            deliverer,
            budget,
            store,
            approvals,
        })
    }

    #[must_use]
    pub fn config(&self) -> &HarnessConfig {
        &self.config
    }

    #[must_use]
    pub fn approvals(&self) -> &ApprovalQueue {
        &self.approvals
    }

    #[must_use]
    pub fn budget(&self) -> &BudgetGuard {
        &self.budget
    }

    /// `<state>/audit.jsonl`.
    #[must_use]
    pub fn audit_path(&self) -> PathBuf {
        self.config.state_dir.join("audit.jsonl")
    }

    fn kill_switch(&self) -> bool {
        self.config.kill_switch_path().exists()
    }

    fn finish(
        audit: &mut AuditLog,
        mut report: RunReport,
        status: RunStatus,
        detail: Option<String>,
    ) -> Result<RunReport> {
        report.status = status;
        report.detail.extend(detail);
        audit.append(
            &report.run_id,
            AuditEvent::RunFinished {
                status: status.as_str().to_owned(),
            },
        )?;
        Ok(report)
    }

    /// Budget-check, then call, the agent bound to `role`.
    async fn call_role(
        &self,
        role: Role,
        prompt: &str,
        run_id: &str,
        audit: &mut AuditLog,
    ) -> Result<Result<String, Stop>> {
        let binding = self.config.roles.binding(role);
        let decision = self.budget.check(&binding.model, Utc::now());
        let detail = match &decision {
            BudgetDecision::Allowed { remaining_usd, .. } => {
                format!("${remaining_usd:.4} remaining this month")
            }
            BudgetDecision::Denied { reason, .. } => reason.clone(),
        };
        audit.append(
            run_id,
            AuditEvent::BudgetChecked {
                role,
                model: binding.model.clone(),
                allowed: decision.is_allowed(),
                detail: detail.clone(),
            },
        )?;
        if !decision.is_allowed() {
            return Ok(Err(Stop {
                status: RunStatus::BudgetDenied,
                detail,
            }));
        }
        match self.agents.send(&binding.agent, prompt).await {
            Ok(reply) => {
                audit.append(
                    run_id,
                    AuditEvent::AgentCalled {
                        role,
                        agent: binding.agent.clone(),
                        model: binding.model.clone(),
                        prompt_sha256: sha256_hex(prompt.as_bytes()),
                        reply_sha256: sha256_hex(reply.as_bytes()),
                    },
                )?;
                Ok(Ok(reply))
            }
            Err(err) => {
                let error = format!("{err:#}");
                audit.append(
                    run_id,
                    AuditEvent::AgentFailed {
                        role,
                        agent: binding.agent.clone(),
                        error: error.clone(),
                    },
                )?;
                Ok(Err(Stop {
                    status: RunStatus::AgentFailed,
                    detail: error,
                }))
            }
        }
    }

    /// Run one subscription once.
    pub async fn run(
        &self,
        subscription: &Subscription,
        audit: &mut AuditLog,
    ) -> Result<RunReport> {
        let pack = self
            .packs
            .get(&subscription.pack)
            .with_context(|| format!("pack {} is not loaded", subscription.pack))?;
        let run_id = format!("run-{}", uuid::Uuid::new_v4());
        let report = RunReport {
            run_id: run_id.clone(),
            subscription: subscription.id.clone(),
            status: RunStatus::NoChanges,
            changes: 0,
            detail: Vec::new(),
            receipt: None,
            approval_id: None,
            held_path: None,
        };
        audit.append(
            &run_id,
            AuditEvent::RunStarted {
                subscription: subscription.id.clone(),
                pack: pack.id.clone(),
                delivery: subscription.delivery,
            },
        )?;
        if self.kill_switch() {
            audit.append(&run_id, AuditEvent::KillSwitchEngaged)?;
            return Self::finish(audit, report, RunStatus::KillSwitch, None);
        }

        let (changes, mut report) = self.collect(subscription, pack, report, audit).await?;
        report.changes = changes.len();
        if changes.is_empty() {
            return Self::finish(audit, report, RunStatus::NoChanges, None);
        }
        let all: Vec<(&SourceSpec, &Change)> = changes.iter().map(|(s, c)| (*s, c)).collect();
        let max_chars = self.config.fetch.max_evidence_chars;

        // Analyst: triage.
        let prompt = analyst_prompt(pack, &render_changes(&all, max_chars), all.len());
        let reply = match self
            .call_role(Role::Analyst, &prompt, &run_id, audit)
            .await?
        {
            Ok(reply) => reply,
            Err(stop) => return Self::finish(audit, report, stop.status, Some(stop.detail)),
        };
        let triage = match parse_triage(&reply, all.len()) {
            Ok(triage) => triage,
            Err(err) => {
                let reason = format!("{err:#}");
                audit.append(
                    &run_id,
                    AuditEvent::BriefRejected {
                        reason: reason.clone(),
                    },
                )?;
                return Self::finish(audit, report, RunStatus::BriefRejected, Some(reason));
            }
        };
        let mut picked: Vec<usize> = triage.relevant.iter().map(|t| t.change).collect();
        picked.sort_unstable();
        audit.append(
            &run_id,
            AuditEvent::Triaged {
                relevant_changes: picked.clone(),
            },
        )?;
        if picked.is_empty() {
            return Self::finish(audit, report, RunStatus::NothingRelevant, None);
        }
        let relevant: Vec<(&SourceSpec, &Change)> = picked.iter().map(|n| all[n - 1]).collect();

        // Writer: the cited brief.
        let prompt = writer_prompt(pack, &render_changes(&relevant, max_chars));
        let reply = match self
            .call_role(Role::Writer, &prompt, &run_id, audit)
            .await?
        {
            Ok(reply) => reply,
            Err(stop) => return Self::finish(audit, report, stop.status, Some(stop.detail)),
        };
        let brief = match parse_brief(&reply, pack.max_items) {
            Ok(brief) => brief,
            Err(err) => {
                let reason = format!("{err:#}");
                audit.append(
                    &run_id,
                    AuditEvent::BriefRejected {
                        reason: reason.clone(),
                    },
                )?;
                return Self::finish(audit, report, RunStatus::BriefRejected, Some(reason));
            }
        };

        // Gate 1: deterministic citations.
        let evidence = EvidenceSet::from_changes(relevant.iter().map(|(_, c)| *c), &self.store)?;
        let citations = verify_brief(&brief, &evidence);
        let mut failures = citations.failures();
        audit.append(
            &run_id,
            AuditEvent::CitationCheck {
                passed: citations.passed,
                failures: failures.clone(),
            },
        )?;

        // Gate 2: cross-vendor review, only once citations hold.
        let semantic_passed = if citations.passed {
            match self
                .call_role(Role::Verifier, &verifier_prompt(&brief), &run_id, audit)
                .await?
            {
                Ok(reply) => match parse_semantic_review(&reply, brief.items.len()) {
                    Ok(review) => {
                        let unsupported: Vec<usize> = review
                            .verdicts
                            .iter()
                            .filter(|v| !v.supported)
                            .map(|v| v.item)
                            .collect();
                        failures.extend(review.verdicts.iter().filter(|v| !v.supported).map(|v| {
                            format!(
                                "item {}: reviewer found it unsupported: {}",
                                v.item, v.reason
                            )
                        }));
                        audit.append(
                            &run_id,
                            AuditEvent::SemanticCheck {
                                passed: unsupported.is_empty(),
                                unsupported_items: unsupported.clone(),
                            },
                        )?;
                        Some(unsupported.is_empty())
                    }
                    Err(err) => {
                        failures.push(format!("reviewer reply rejected: {err:#}"));
                        audit.append(
                            &run_id,
                            AuditEvent::SemanticCheck {
                                passed: false,
                                unsupported_items: Vec::new(),
                            },
                        )?;
                        Some(false)
                    }
                },
                Err(stop) => {
                    failures.push(stop.detail);
                    None
                }
            }
        } else {
            None
        };

        let envelope = self.envelope(subscription, pack, &run_id, brief, &relevant);
        let gates = GateResults {
            citations_passed: citations.passed,
            semantic_passed,
            failures,
        };
        let decision = decide(subscription.delivery, &gates, self.kill_switch());
        let reasons = match &decision {
            DeliveryDecision::Hold { reasons } => reasons.clone(),
            _ => Vec::new(),
        };
        audit.append(
            &run_id,
            AuditEvent::DeliveryDecided {
                decision: decision.label().to_owned(),
                reasons: reasons.clone(),
            },
        )?;

        match decision {
            DeliveryDecision::Send => {
                match self
                    .deliverer
                    .deliver(&subscription.destination, &envelope)
                    .await
                {
                    Ok(receipt) => {
                        audit.append(
                            &run_id,
                            AuditEvent::Delivered {
                                receipt: receipt.clone(),
                                brief_sha256: envelope.brief_sha256()?,
                            },
                        )?;
                        let mut report = report;
                        report.receipt = Some(receipt);
                        Self::finish(audit, report, RunStatus::Delivered, None)
                    }
                    Err(err) => {
                        let error = format!("{err:#}");
                        audit.append(
                            &run_id,
                            AuditEvent::DeliveryFailed {
                                error: error.clone(),
                            },
                        )?;
                        let path = self.hold(&envelope, std::slice::from_ref(&error), audit)?;
                        let mut report = report;
                        report.held_path = Some(path);
                        Self::finish(audit, report, RunStatus::DeliveryFailed, Some(error))
                    }
                }
            }
            DeliveryDecision::QueueForApproval => {
                let id = self.approvals.enqueue(&envelope)?;
                audit.append(
                    &run_id,
                    AuditEvent::QueuedForApproval {
                        approval_id: id.clone(),
                    },
                )?;
                let mut report = report;
                report.approval_id = Some(id);
                Self::finish(audit, report, RunStatus::QueuedForApproval, None)
            }
            DeliveryDecision::Hold { reasons } => {
                let path = self.hold(&envelope, &reasons, audit)?;
                let mut report = report;
                report.held_path = Some(path);
                report.detail.extend(reasons);
                Self::finish(audit, report, RunStatus::Held, None)
            }
        }
    }

    /// Fetch and classify every source in the pack.
    async fn collect<'p>(
        &self,
        subscription: &Subscription,
        pack: &'p VerticalPack,
        mut report: RunReport,
        audit: &mut AuditLog,
    ) -> Result<(Vec<(&'p SourceSpec, Change)>, RunReport)> {
        let mut changes = Vec::new();
        for source in &pack.sources {
            let text = match self.fetcher.fetch(source).await {
                Ok(doc) => normalize(&doc),
                Err(err) => {
                    let error = format!("{err:#}");
                    report.detail.push(format!("{}: {error}", source.id));
                    audit.append(
                        &report.run_id,
                        AuditEvent::SourceFailed {
                            source_id: source.id.clone(),
                            error,
                        },
                    )?;
                    continue;
                }
            };
            if text.is_empty() {
                let error = "no text after normalization".to_owned();
                report.detail.push(format!("{}: {error}", source.id));
                audit.append(
                    &report.run_id,
                    AuditEvent::SourceFailed {
                        source_id: source.id.clone(),
                        error,
                    },
                )?;
                continue;
            }
            match self
                .store
                .record(&subscription.id, source, &text, Utc::now())?
            {
                ChangeOutcome::Baseline(meta) => audit.append(
                    &report.run_id,
                    AuditEvent::SourceBaseline {
                        source_id: source.id.clone(),
                        snapshot: meta.sha256,
                    },
                )?,
                ChangeOutcome::Unchanged(meta) => audit.append(
                    &report.run_id,
                    AuditEvent::SourceUnchanged {
                        source_id: source.id.clone(),
                        snapshot: meta.sha256,
                    },
                )?,
                ChangeOutcome::Changed(change) => {
                    let entry = audit.append(
                        &report.run_id,
                        AuditEvent::SourceChanged {
                            source_id: source.id.clone(),
                            previous: change.previous.sha256.clone(),
                            current: change.current.sha256.clone(),
                            added_lines: change.added.len(),
                            removed_lines: change.removed.len(),
                        },
                    )?;
                    changes.push((source, change));
                    entry
                }
            };
        }
        Ok((changes, report))
    }

    fn envelope(
        &self,
        subscription: &Subscription,
        pack: &VerticalPack,
        run_id: &str,
        brief: crate::brief::Brief,
        relevant: &[(&SourceSpec, &Change)],
    ) -> DeliveryEnvelope {
        let generated_at = Utc::now();
        let mut sources = Vec::new();
        for (source, change) in relevant {
            for meta in [&change.current, &change.previous] {
                sources.push(SourceRef {
                    source_id: source.id.clone(),
                    label: source.label.clone(),
                    url: source.url.to_string(),
                    snapshot: meta.sha256.clone(),
                    captured_at: meta.captured_at,
                });
            }
        }
        let markdown = render_markdown(&pack.name, generated_at, &brief, &sources);
        DeliveryEnvelope {
            run_id: run_id.to_owned(),
            subscription: subscription.id.clone(),
            pack: pack.id.clone(),
            pack_name: pack.name.clone(),
            generated_at,
            brief,
            sources,
            markdown,
        }
    }

    /// Keep a held brief for inspection under `<state>/held/`.
    fn hold(
        &self,
        envelope: &DeliveryEnvelope,
        reasons: &[String],
        audit: &mut AuditLog,
    ) -> Result<String> {
        let path = self
            .config
            .state_dir
            .join("held")
            .join(format!("{}.json", envelope.run_id));
        write_json_atomic(&path, &HeldBrief { reasons, envelope })?;
        let shown = path.display().to_string();
        audit.append(
            &envelope.run_id,
            AuditEvent::Held {
                path: shown.clone(),
            },
        )?;
        Ok(shown)
    }

    /// Approve a queued brief: deliver it to the subscription's current
    /// destination, then mark it approved. A failed delivery leaves it
    /// pending.
    pub async fn approve(&self, id: &str, by: &str, audit: &mut AuditLog) -> Result<String> {
        if by.trim().is_empty() {
            bail!("an approver name is required");
        }
        let item = self.approvals.get(id)?;
        let run_id = item.envelope.run_id.clone();
        if self.kill_switch() {
            audit.append(&run_id, AuditEvent::KillSwitchEngaged)?;
            bail!("the kill switch is engaged; nothing is sent while it exists");
        }
        let subscription = self
            .config
            .subscription(&item.subscription)
            .with_context(|| {
                format!(
                    "subscription {} is no longer configured; reject this item instead",
                    item.subscription
                )
            })?;
        match self
            .deliverer
            .deliver(&subscription.destination, &item.envelope)
            .await
        {
            Ok(receipt) => {
                self.approvals.resolve(id, Resolution::Approved)?;
                audit.append(
                    &run_id,
                    AuditEvent::ApprovalResolved {
                        approval_id: id.to_owned(),
                        approved: true,
                        by: by.to_owned(),
                        reason: None,
                    },
                )?;
                audit.append(
                    &run_id,
                    AuditEvent::Delivered {
                        receipt: receipt.clone(),
                        brief_sha256: item.envelope.brief_sha256()?,
                    },
                )?;
                Ok(receipt)
            }
            Err(err) => {
                audit.append(
                    &run_id,
                    AuditEvent::DeliveryFailed {
                        error: format!("{err:#}"),
                    },
                )?;
                Err(err.context(format!("delivering approval {id}; it is still pending")))
            }
        }
    }

    /// Reject a queued brief. It is kept under `approvals/rejected/`.
    pub fn reject(
        &self,
        id: &str,
        by: &str,
        reason: Option<&str>,
        audit: &mut AuditLog,
    ) -> Result<()> {
        if by.trim().is_empty() {
            bail!("an approver name is required");
        }
        let item = self.approvals.get(id)?;
        self.approvals.resolve(id, Resolution::Rejected)?;
        audit.append(
            &item.envelope.run_id,
            AuditEvent::ApprovalResolved {
                approval_id: id.to_owned(),
                approved: false,
                by: by.to_owned(),
                reason: reason.map(str::to_owned),
            },
        )?;
        Ok(())
    }
}
