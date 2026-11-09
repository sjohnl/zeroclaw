//! Delivery policy and transports.
//!
//! The policy is one pure function, [`decide`]: a brief that failed any
//! gate is held in every mode, the kill switch holds everything, and the
//! subscription's mode chooses between sending now and queueing for
//! sign-off. Transports only run after that decision.

use std::future::Future;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::brief::Brief;
use crate::config::Destination;
use crate::fsutil::{sha256_hex, write_json_atomic};

/// How a subscription's verified briefs leave the harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    /// Sent as soon as every gate passes; no human approves individual
    /// briefs.
    Autonomous,
    /// Queued after every gate passes; sent only once an operator approves.
    Approval,
}

/// Outcome of the verification gates for one brief.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateResults {
    pub citations_passed: bool,
    /// Cross-vendor review result; `None` when the review did not run
    /// (citations failed first, its budget was denied, or the call failed).
    pub semantic_passed: Option<bool>,
    pub failures: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum DeliveryDecision {
    Send,
    QueueForApproval,
    Hold { reasons: Vec<String> },
}

impl DeliveryDecision {
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::QueueForApproval => "queue_for_approval",
            Self::Hold { .. } => "hold",
        }
    }
}

/// The delivery policy.
#[must_use]
pub fn decide(mode: DeliveryMode, gates: &GateResults, kill_switch: bool) -> DeliveryDecision {
    let mut reasons = Vec::new();
    if kill_switch {
        reasons.push("kill switch is engaged".to_owned());
    }
    if !gates.citations_passed {
        reasons.push("citation check failed".to_owned());
    }
    match gates.semantic_passed {
        Some(true) => {}
        Some(false) => reasons.push("cross-vendor review failed".to_owned()),
        None => reasons.push("cross-vendor review did not run".to_owned()),
    }
    if !reasons.is_empty() {
        reasons.extend(gates.failures.iter().cloned());
        return DeliveryDecision::Hold { reasons };
    }
    match mode {
        DeliveryMode::Autonomous => DeliveryDecision::Send,
        DeliveryMode::Approval => DeliveryDecision::QueueForApproval,
    }
}

/// A cited source as shown to the subscriber.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceRef {
    pub source_id: String,
    pub label: String,
    pub url: String,
    pub snapshot: String,
    pub captured_at: DateTime<Utc>,
}

/// What a destination receives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeliveryEnvelope {
    pub run_id: String,
    pub subscription: String,
    pub pack: String,
    pub pack_name: String,
    pub generated_at: DateTime<Utc>,
    pub brief: Brief,
    pub sources: Vec<SourceRef>,
    /// Ready-to-post rendering of `brief` with its citations.
    pub markdown: String,
}

impl DeliveryEnvelope {
    /// SHA-256 of the brief's JSON, recorded in the audit log.
    pub fn brief_sha256(&self) -> Result<String> {
        Ok(sha256_hex(
            &serde_json::to_vec(&self.brief).context("serializing brief")?,
        ))
    }
}

/// Render a brief as Markdown with each claim's quotes and links.
#[must_use]
pub fn render_markdown(
    pack_name: &str,
    generated_at: DateTime<Utc>,
    brief: &Brief,
    sources: &[SourceRef],
) -> String {
    let mut out = format!(
        "# {}\n\n_{} · {}_\n\n",
        brief.headline.trim(),
        pack_name,
        generated_at.format("%Y-%m-%d %H:%M UTC")
    );
    for (n, item) in brief.items.iter().enumerate() {
        out.push_str(&format!(
            "{}. **{}** {}\n",
            n + 1,
            item.claim.trim(),
            item.why_it_matters.trim()
        ));
        for citation in &item.citations {
            let source = sources
                .iter()
                .find(|s| s.snapshot == citation.snapshot && s.source_id == citation.source_id);
            match source {
                Some(s) => out.push_str(&format!(
                    "   > \"{}\" ([{}]({}), captured {})\n",
                    citation.quote.trim(),
                    s.label,
                    s.url,
                    s.captured_at.format("%Y-%m-%d")
                )),
                None => out.push_str(&format!(
                    "   > \"{}\" ({})\n",
                    citation.quote.trim(),
                    citation.source_id
                )),
            }
        }
        out.push('\n');
    }
    out
}

/// Sends an envelope to a destination and returns a receipt.
pub trait Deliverer {
    fn deliver(
        &self,
        destination: &Destination,
        envelope: &DeliveryEnvelope,
    ) -> impl Future<Output = Result<String>> + Send;
}

/// Production transports: outbox files and HTTPS webhooks.
#[derive(Debug, Clone)]
pub struct LiveDeliverer {
    client: reqwest::Client,
}

impl LiveDeliverer {
    pub fn new(timeout_secs: u64) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self { client })
    }
}

/// Outbox file name: `<dir>/<subscription>/<timestamp>-<brief hash prefix>.json`.
pub fn outbox_path(dir: &std::path::Path, envelope: &DeliveryEnvelope) -> Result<PathBuf> {
    let sha = envelope.brief_sha256()?;
    Ok(dir.join(&envelope.subscription).join(format!(
        "{}-{}.json",
        envelope.generated_at.format("%Y%m%dT%H%M%SZ"),
        &sha[..12]
    )))
}

impl Deliverer for LiveDeliverer {
    async fn deliver(
        &self,
        destination: &Destination,
        envelope: &DeliveryEnvelope,
    ) -> Result<String> {
        match destination {
            Destination::Outbox { dir } => {
                let path = outbox_path(dir, envelope)?;
                write_json_atomic(&path, envelope)?;
                Ok(format!("outbox:{}", path.display()))
            }
            Destination::Webhook { url, token_env } => {
                let mut request = self.client.post(url.clone()).json(envelope);
                if let Some(name) = token_env {
                    let token = std::env::var(name)
                        .with_context(|| format!("webhook token variable {name} is not set"))?;
                    request = request.bearer_auth(token);
                }
                let response = request
                    .send()
                    .await
                    .with_context(|| format!("posting to {url}"))?;
                let status = response.status();
                if !status.is_success() {
                    bail!("webhook {url} answered {status}");
                }
                Ok(format!("webhook:{url} {status}"))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::brief::{BriefItem, Citation};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn passed() -> GateResults {
        GateResults {
            citations_passed: true,
            semantic_passed: Some(true),
            failures: vec![],
        }
    }

    #[test]
    fn policy_matrix() {
        assert_eq!(
            decide(DeliveryMode::Autonomous, &passed(), false),
            DeliveryDecision::Send
        );
        assert_eq!(
            decide(DeliveryMode::Approval, &passed(), false),
            DeliveryDecision::QueueForApproval
        );
        for mode in [DeliveryMode::Autonomous, DeliveryMode::Approval] {
            assert!(matches!(
                decide(mode, &passed(), true),
                DeliveryDecision::Hold { .. }
            ));
            let failed = GateResults {
                citations_passed: false,
                semantic_passed: None,
                failures: vec!["item 1: quote not found".into()],
            };
            let DeliveryDecision::Hold { reasons } = decide(mode, &failed, false) else {
                panic!("a failed gate must hold in {mode:?} mode");
            };
            assert!(reasons.iter().any(|r| r.contains("quote not found")));
            for review in [Some(false), None] {
                let semantic = GateResults {
                    citations_passed: true,
                    semantic_passed: review,
                    failures: vec![],
                };
                assert!(matches!(
                    decide(mode, &semantic, false),
                    DeliveryDecision::Hold { .. }
                ));
            }
        }
    }

    pub(crate) fn envelope() -> DeliveryEnvelope {
        let at = Utc::now();
        let brief = Brief {
            headline: "One new bid".into(),
            items: vec![BriefItem {
                claim: "City opened a network RFP".into(),
                why_it_matters: "It matches your services.".into(),
                citations: vec![Citation {
                    source_id: "city-bids".into(),
                    snapshot: "a".repeat(64),
                    quote: "Network refresh RFP".into(),
                }],
            }],
        };
        let sources = vec![SourceRef {
            source_id: "city-bids".into(),
            label: "City bid board".into(),
            url: "https://bids.example.gov/open".into(),
            snapshot: "a".repeat(64),
            captured_at: at,
        }];
        let markdown = render_markdown("RFP scouting", at, &brief, &sources);
        DeliveryEnvelope {
            run_id: "run-1".into(),
            subscription: "acme-rfp".into(),
            pack: "rfp-scouting".into(),
            pack_name: "RFP scouting".into(),
            generated_at: at,
            brief,
            sources,
            markdown,
        }
    }

    #[test]
    fn markdown_carries_quote_and_link() {
        let md = envelope().markdown;
        assert!(md.starts_with("# One new bid"));
        assert!(md.contains("\"Network refresh RFP\""));
        assert!(md.contains("(https://bids.example.gov/open)"));
    }

    #[tokio::test]
    async fn outbox_writes_envelope_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = envelope();
        let receipt = LiveDeliverer::new(5)
            .unwrap()
            .deliver(
                &Destination::Outbox {
                    dir: dir.path().to_path_buf(),
                },
                &env,
            )
            .await
            .unwrap();
        let path = outbox_path(dir.path(), &env).unwrap();
        assert!(receipt.contains(&path.display().to_string()));
        let stored: DeliveryEnvelope =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(stored, env);
    }

    #[tokio::test]
    async fn webhook_posts_with_bearer_and_fails_on_error_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/ok"))
            .and(header("authorization", "Bearer hook-secret"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/down"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        // The variable name is unique to this test, so setting it cannot
        // race another test reading the same name.
        // SAFETY: no other thread reads or writes this variable.
        unsafe { std::env::set_var("BRIEF_HARNESS_TEST_HOOK_TOKEN", "hook-secret") };
        let deliverer = LiveDeliverer::new(5).unwrap();
        let ok = Destination::Webhook {
            url: format!("{}/ok", server.uri()).parse().unwrap(),
            token_env: Some("BRIEF_HARNESS_TEST_HOOK_TOKEN".into()),
        };
        deliverer.deliver(&ok, &envelope()).await.unwrap();
        let down = Destination::Webhook {
            url: format!("{}/down", server.uri()).parse().unwrap(),
            token_env: None,
        };
        assert!(deliverer.deliver(&down, &envelope()).await.is_err());
    }
}
