//! End-to-end runs of the harness with scripted sources and agents.
//!
//! Real snapshot store, budget guard (over a real ledger file written with
//! ZeroClaw's own `CostRecord`), verification, approval queue, outbox
//! delivery, and audit chain. Only the network edges are fakes: the
//! fetcher returns scripted pages and the agent client returns scripted
//! replies while recording which agents were called.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, bail};
use chrono::Utc;
use serde_json::json;

use brief_harness::a2a::AgentClient;
use brief_harness::audit::{self, AuditLog};
use brief_harness::config::HarnessConfig;
use brief_harness::delivery::{DeliveryEnvelope, LiveDeliverer};
use brief_harness::fsutil::sha256_hex;
use brief_harness::pack::{SourceSpec, load_packs_dir};
use brief_harness::pipeline::{Harness, RunStatus};
use brief_harness::snapshot::{FetchedDoc, Fetcher, normalize};
use zeroclaw_config::cost::{CostRecord, TokenUsage};

const PACK: &str = r#"
id = "rfp-scouting"
name = "RFP and grant scouting"
description = "Public notices for small IT firms."
max_items = 3

[analyst]
instructions = "Keep notices an IT services firm could bid on."

[writer]
instructions = "Lead with deadline and value."

[[sources]]
id = "city-bids"
label = "City bid board"
url = "https://bids.example.gov/open"

[[sources]]
id = "state-grants"
label = "State grants"
url = "https://grants.example.gov/list"
"#;

const BIDS_V1: &str = "Open bids\nJanitorial services, closes 2026-11-20";
const GRANTS_V1: &str = "Grant programs\nSmall business resilience grant";
const GRANTS_V2: &str = "Grant programs\nSmall business resilience grant\nRural broadband grant opens 2026-12-15 for ISPs";
const NEW_LINE: &str = "Rural broadband grant opens 2026-12-15 for ISPs";

#[derive(Default)]
struct FakeFetcher {
    pages: Mutex<HashMap<String, String>>,
}

impl FakeFetcher {
    fn set(&self, id: &str, text: &str) {
        self.pages.lock().unwrap().insert(id.into(), text.into());
    }
}

impl Fetcher for &FakeFetcher {
    async fn fetch(&self, source: &SourceSpec) -> Result<FetchedDoc> {
        let Some(body) = self.pages.lock().unwrap().get(&source.id).cloned() else {
            bail!("no page scripted for {}", source.id);
        };
        Ok(FetchedDoc {
            body,
            content_type: Some("text/plain".into()),
        })
    }
}

#[derive(Default)]
struct FakeAgents {
    replies: Mutex<HashMap<String, VecDeque<String>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeAgents {
    fn script(&self, agent: &str, reply: serde_json::Value) {
        self.replies
            .lock()
            .unwrap()
            .entry(agent.into())
            .or_default()
            .push_back(reply.to_string());
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl AgentClient for &FakeAgents {
    async fn send(&self, agent: &str, text: &str) -> Result<String> {
        assert!(
            text.contains("<untrusted_source_data>"),
            "source data must reach agents framed"
        );
        self.calls.lock().unwrap().push(agent.into());
        let Some(reply) = self
            .replies
            .lock()
            .unwrap()
            .get_mut(agent)
            .and_then(VecDeque::pop_front)
        else {
            bail!("no scripted reply for {agent}");
        };
        Ok(reply)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: HarnessConfig,
    fetcher: FakeFetcher,
    agents: FakeAgents,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        fs::create_dir_all(root.join("packs")).unwrap();
        fs::write(root.join("packs/rfp-scouting.toml"), PACK).unwrap();
        fs::write(root.join("costs.jsonl"), "").unwrap();
        let config_text = format!(
            r#"
state_dir = "state"
packs_dir = "packs"

[zeroclaw]
gateway_url = "http://127.0.0.1:42617"
token_env = "UNUSED_IN_TESTS"
cost_ledger = "{ledger}"

[roles.analyst]
agent = "brief_analyst"
model = "claude-haiku-4-5"
vendor = "anthropic"

[roles.writer]
agent = "brief_writer"
model = "claude-sonnet-4-5"
vendor = "anthropic"

[roles.verifier]
agent = "brief_verifier"
model = "gpt-5-mini"
vendor = "openai"

[[subscriptions]]
id = "auto"
pack = "rfp-scouting"
delivery = "autonomous"
destination = {{ kind = "outbox", dir = "outbox" }}

[[subscriptions]]
id = "gated"
pack = "rfp-scouting"
delivery = "approval"
destination = {{ kind = "outbox", dir = "outbox" }}
"#,
            ledger = root.join("costs.jsonl").display()
        );
        fs::write(root.join("brief-harness.toml"), config_text).unwrap();
        let config = HarnessConfig::load(&root.join("brief-harness.toml")).unwrap();
        let fetcher = FakeFetcher::default();
        fetcher.set("city-bids", BIDS_V1);
        fetcher.set("state-grants", GRANTS_V1);
        Self {
            _dir: dir,
            root,
            config,
            fetcher,
            agents: FakeAgents::default(),
        }
    }

    fn harness(&self) -> Harness<&FakeFetcher, &FakeAgents, LiveDeliverer> {
        let packs = load_packs_dir(&self.config.packs_dir).unwrap();
        Harness::new(
            self.config.clone(),
            packs,
            &self.fetcher,
            &self.agents,
            LiveDeliverer::new(5).unwrap(),
        )
        .unwrap()
    }

    fn audit(&self) -> AuditLog {
        AuditLog::open(&self.config.state_dir.join("audit.jsonl")).unwrap()
    }

    fn outbox_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        let dir = self.root.join("outbox");
        if let Ok(subs) = fs::read_dir(&dir) {
            for sub in subs {
                for entry in fs::read_dir(sub.unwrap().path()).unwrap() {
                    files.push(entry.unwrap().path());
                }
            }
        }
        files
    }

    fn spend(&self, model: &str, usd: f64) {
        let mut usage = TokenUsage::new(model, 1_000, 100, 0, 0.0, 0.0, 0.0);
        usage.cost_usd = usd;
        usage.timestamp = Utc::now();
        let record = CostRecord::with_agent("daemon", Some("brief_analyst".into()), usage);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(self.root.join("costs.jsonl"))
            .unwrap();
        writeln!(file, "{}", serde_json::to_string(&record).unwrap()).unwrap();
    }

    /// The hash the harness will store for the changed grants page.
    fn grants_v2_sha(&self) -> String {
        sha256_hex(
            normalize(&FetchedDoc {
                body: GRANTS_V2.into(),
                content_type: Some("text/plain".into()),
            })
            .as_bytes(),
        )
    }

    fn script_happy_path(&self, quote: &str, supported: bool) {
        self.agents.script(
            "brief_analyst",
            json!({"relevant":[{"change":1,"reason":"new grant an ISP can apply for"}]}),
        );
        self.agents.script(
            "brief_writer",
            json!({"headline":"A rural broadband grant opens in December","items":[{
                "claim":"The state lists a rural broadband grant opening 2026-12-15",
                "why_it_matters":"ISP clients can apply.",
                "citations":[{"source_id":"state-grants","snapshot":self.grants_v2_sha(),"quote":quote}]
            }]}),
        );
        self.agents.script(
            "brief_verifier",
            json!({"verdicts":[{"item":1,"supported":supported,"reason":"checked against quote"}]}),
        );
    }
}

fn assert_chain_ok(path: &Path) {
    audit::verify(path).expect("audit chain must verify after every scenario");
}

async fn baseline(
    fx: &Fixture,
    harness: &Harness<&FakeFetcher, &FakeAgents, LiveDeliverer>,
    sub: &str,
) {
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription(sub).unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(
        report.status,
        RunStatus::NoChanges,
        "first run only records a baseline"
    );
}

#[tokio::test]
async fn quiet_sources_cost_nothing() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;
    baseline(&fx, &harness, "auto").await;
    assert!(
        fx.agents.calls().is_empty(),
        "no change means no model call"
    );
    assert_chain_ok(&fx.config.state_dir.join("audit.jsonl"));
}

#[tokio::test]
async fn autonomous_mode_delivers_a_verified_brief() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;

    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();

    assert_eq!(report.status, RunStatus::Delivered, "{:?}", report.detail);
    assert_eq!(report.changes, 1);
    assert_eq!(
        fx.agents.calls(),
        ["brief_analyst", "brief_writer", "brief_verifier"]
    );
    let files = fx.outbox_files();
    assert_eq!(files.len(), 1);
    let envelope: DeliveryEnvelope = serde_json::from_slice(&fs::read(&files[0]).unwrap()).unwrap();
    assert!(envelope.markdown.contains(NEW_LINE));
    assert!(
        envelope
            .markdown
            .contains("https://grants.example.gov/list")
    );
    assert_chain_ok(&fx.config.state_dir.join("audit.jsonl"));
}

#[tokio::test]
async fn approval_mode_queues_then_sends_only_when_approved() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "gated").await;

    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("gated").unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(
        report.status,
        RunStatus::QueuedForApproval,
        "{:?}",
        report.detail
    );
    assert!(
        fx.outbox_files().is_empty(),
        "nothing leaves before approval"
    );

    let id = report.approval_id.unwrap();
    assert!(
        harness.approve(&id, "", &mut log).await.is_err(),
        "approver required"
    );
    harness.approve(&id, "operator-a", &mut log).await.unwrap();
    assert_eq!(fx.outbox_files().len(), 1);
    assert!(harness.approvals().list().unwrap().is_empty());
    assert_chain_ok(&fx.config.state_dir.join("audit.jsonl"));
}

#[tokio::test]
async fn rejected_approval_is_never_sent() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "gated").await;
    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("gated").unwrap(), &mut log)
        .await
        .unwrap();
    let id = report.approval_id.unwrap();
    harness
        .reject(&id, "operator-b", Some("not relevant this week"), &mut log)
        .unwrap();
    assert!(harness.approve(&id, "operator-a", &mut log).await.is_err());
    assert!(fx.outbox_files().is_empty());
}

#[tokio::test]
async fn fabricated_quote_is_held_without_spending_on_review() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;

    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path("Rural broadband grant worth $4M opens", true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();

    assert_eq!(report.status, RunStatus::Held);
    assert_eq!(
        fx.agents.calls(),
        ["brief_analyst", "brief_writer"],
        "the cross-vendor reviewer is not paid for a brief that already failed"
    );
    assert!(fx.outbox_files().is_empty(), "a failed brief is never sent");
    assert!(Path::new(report.held_path.as_deref().unwrap()).exists());
    assert_chain_ok(&fx.config.state_dir.join("audit.jsonl"));
}

#[tokio::test]
async fn reviewer_objection_holds_the_brief() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;
    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, false);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(report.status, RunStatus::Held);
    assert!(report.detail.iter().any(|d| d.contains("unsupported")));
    assert!(fx.outbox_files().is_empty());
}

#[tokio::test]
async fn exhausted_model_budget_stops_before_any_call() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;
    fx.spend("claude-haiku-4-5", 19.90);

    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(report.status, RunStatus::BudgetDenied);
    assert!(fx.agents.calls().is_empty());
    assert!(fx.outbox_files().is_empty());
}

#[tokio::test]
async fn kill_switch_blocks_runs_and_approvals() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "gated").await;
    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let queued = harness
        .run(fx.config.subscription("gated").unwrap(), &mut log)
        .await
        .unwrap();
    let id = queued.approval_id.unwrap();

    fs::write(fx.config.kill_switch_path(), "").unwrap();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(report.status, RunStatus::KillSwitch);
    assert!(harness.approve(&id, "operator-a", &mut log).await.is_err());
    assert!(fx.outbox_files().is_empty());

    fs::remove_file(fx.config.kill_switch_path()).unwrap();
    harness.approve(&id, "operator-a", &mut log).await.unwrap();
    assert_eq!(fx.outbox_files().len(), 1);
}

#[tokio::test]
async fn unreachable_source_is_audited_and_others_still_run() {
    let fx = Fixture::new();
    let harness = fx.harness();
    baseline(&fx, &harness, "auto").await;
    fx.fetcher.pages.lock().unwrap().remove("city-bids");
    fx.fetcher.set("state-grants", GRANTS_V2);
    fx.script_happy_path(NEW_LINE, true);
    let mut log = fx.audit();
    let report = harness
        .run(fx.config.subscription("auto").unwrap(), &mut log)
        .await
        .unwrap();
    assert_eq!(report.status, RunStatus::Delivered);
    assert!(report.detail.iter().any(|d| d.starts_with("city-bids")));
}
