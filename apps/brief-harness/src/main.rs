//! `brief-harness` command line. Every command prints JSON to stdout so a
//! scheduler, a dashboard, or an operator's shell can consume it.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::json;

use brief_harness::a2a::A2aClient;
use brief_harness::audit::{self, AuditLog};
use brief_harness::config::HarnessConfig;
use brief_harness::delivery::LiveDeliverer;
use brief_harness::fsutil::RunLock;
use brief_harness::pack::load_packs_dir;
use brief_harness::pipeline::Harness;
use brief_harness::snapshot::HttpFetcher;

#[derive(Debug, Parser)]
#[command(
    name = "brief-harness",
    version,
    about = "Cited intelligence briefs on ZeroClaw agents"
)]
struct Cli {
    /// Path to brief-harness.toml.
    #[arg(long, short, env = "BRIEF_HARNESS_CONFIG")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Load and cross-check the config and every pack.
    Validate,
    /// Run one subscription, or all of them.
    Run {
        #[arg(long)]
        subscription: Option<String>,
    },
    /// Month-to-date spend and headroom for each role's model.
    Budget,
    /// Work the approval queue.
    Approvals {
        #[command(subcommand)]
        action: ApprovalAction,
    },
    /// Verify the audit log's hash chain.
    AuditVerify,
}

#[derive(Debug, Subcommand)]
enum ApprovalAction {
    List,
    Approve {
        id: String,
        #[arg(long)]
        by: String,
    },
    Reject {
        id: String,
        #[arg(long)]
        by: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

fn print(value: &serde_json::Value) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("rendering output")?
    );
    Ok(())
}

fn harness(
    config: HarnessConfig,
    with_agents: bool,
) -> Result<Harness<HttpFetcher, A2aClient, LiveDeliverer>> {
    let packs = load_packs_dir(&config.packs_dir)?;
    let fetcher = HttpFetcher::new(&config.fetch)?;
    let token = if with_agents {
        std::env::var(&config.zeroclaw.token_env).with_context(|| {
            format!(
                "set {} to the ZeroClaw gateway pairing token",
                config.zeroclaw.token_env
            )
        })?
    } else {
        // Commands that never call an agent do not need the token.
        "unused".to_owned()
    };
    let agents = A2aClient::new(
        config.zeroclaw.gateway_url.clone(),
        token,
        config.zeroclaw.agent_timeout_secs,
    )?;
    let deliverer = LiveDeliverer::new(config.fetch.timeout_secs)?;
    Harness::new(config, packs, fetcher, agents, deliverer)
}

async fn execute(cli: Cli) -> Result<()> {
    let config = HarnessConfig::load(&cli.config)?;
    match cli.command {
        Command::Validate => {
            let packs = load_packs_dir(&config.packs_dir)?;
            config.validate_against_packs(&packs)?;
            print(&json!({
                "valid": true,
                "packs": packs.keys().collect::<Vec<_>>(),
                "subscriptions": config.subscriptions.iter().map(|s| &s.id).collect::<Vec<_>>(),
            }))
        }
        Command::Run { subscription } => {
            let _lock = RunLock::acquire(&config.state_dir)?;
            let harness = harness(config, true)?;
            let mut log = AuditLog::open(&harness.audit_path())?;
            let targets: Vec<_> = match &subscription {
                Some(id) => vec![
                    harness
                        .config()
                        .subscription(id)
                        .with_context(|| format!("no subscription {id}"))?
                        .clone(),
                ],
                None => harness.config().subscriptions.clone(),
            };
            let mut reports = Vec::new();
            for sub in &targets {
                reports.push(harness.run(sub, &mut log).await?);
            }
            print(&serde_json::to_value(&reports).context("rendering reports")?)
        }
        Command::Budget => {
            let harness = harness(config, false)?;
            let now = Utc::now();
            let mut rows = Vec::new();
            for model in harness.config().roles.models() {
                let row = match harness.budget().spend(model, now) {
                    Ok(spend) => json!({
                        "model": model,
                        "spent_usd": spend.cost_usd,
                        "cap_usd": harness.budget().monthly_cap_usd(),
                        "remaining_usd": harness.budget().monthly_cap_usd() - spend.cost_usd,
                        "unpriced_tokens": spend.unpriced_tokens,
                        "requests": spend.requests,
                        "allowed": harness.budget().check(model, now).is_allowed(),
                    }),
                    Err(err) => {
                        json!({ "model": model, "error": format!("{err:#}"), "allowed": false })
                    }
                };
                rows.push(row);
            }
            print(&json!(rows))
        }
        Command::Approvals { action } => {
            let _lock = RunLock::acquire(&config.state_dir)?;
            let harness = harness(config, false)?;
            match action {
                ApprovalAction::List => {
                    let items: Vec<_> = harness
                        .approvals()
                        .list()?
                        .into_iter()
                        .map(|item| {
                            json!({
                                "id": item.id,
                                "subscription": item.subscription,
                                "created_at": item.created_at,
                                "headline": item.envelope.brief.headline,
                                "markdown": item.envelope.markdown,
                            })
                        })
                        .collect();
                    print(&json!(items))
                }
                ApprovalAction::Approve { id, by } => {
                    let mut log = AuditLog::open(&harness.audit_path())?;
                    let receipt = harness.approve(&id, &by, &mut log).await?;
                    print(&json!({ "approved": id, "receipt": receipt }))
                }
                ApprovalAction::Reject { id, by, reason } => {
                    let mut log = AuditLog::open(&harness.audit_path())?;
                    harness.reject(&id, &by, reason.as_deref(), &mut log)?;
                    print(&json!({ "rejected": id }))
                }
            }
        }
        Command::AuditVerify => {
            let path = config.state_dir.join("audit.jsonl");
            match audit::verify(&path) {
                Ok(summary) => print(
                    &json!({ "valid": true, "entries": summary.entries, "head": summary.head }),
                ),
                Err(err) => {
                    print(&json!({ "valid": false, "error": format!("{err:#}") }))?;
                    bail!("audit chain does not verify")
                }
            }
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match execute(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("brief-harness: {err:#}");
            ExitCode::FAILURE
        }
    }
}
