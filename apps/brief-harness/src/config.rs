//! Harness configuration (`brief-harness.toml`).
//!
//! The config names the ZeroClaw gateway and cost ledger the harness relies
//! on, binds each pipeline role to one ZeroClaw agent and model, sets the
//! per-model budget, and lists subscriptions. Secrets never live here: the
//! gateway token and webhook tokens are read from the environment variables
//! the config names.

use std::collections::BTreeSet;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

use crate::delivery::DeliveryMode;
use crate::pack::{VerticalPack, is_slug};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Snapshots, audit log, approvals, and held briefs live here.
    pub state_dir: PathBuf,
    /// Directory of vertical pack TOML files.
    pub packs_dir: PathBuf,
    pub zeroclaw: ZeroClawLink,
    pub roles: Roles,
    #[serde(default)]
    pub budget: BudgetPolicy,
    #[serde(default)]
    pub fetch: FetchPolicy,
    #[serde(default)]
    pub subscriptions: Vec<Subscription>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZeroClawLink {
    /// Base URL of the ZeroClaw gateway, for example `http://127.0.0.1:42617`.
    pub gateway_url: Url,
    /// Name of the environment variable holding the gateway bearer token.
    pub token_env: String,
    /// The daemon's cost ledger, `<workspace>/state/costs.jsonl`. The budget
    /// guard reads spend from here; the harness keeps no spend counter of
    /// its own.
    pub cost_ledger: PathBuf,
    #[serde(default = "default_agent_timeout_secs")]
    pub agent_timeout_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    pub analyst: RoleBinding,
    pub writer: RoleBinding,
    pub verifier: RoleBinding,
}

/// One pipeline role bound to a ZeroClaw agent alias and the model that
/// agent is configured to use.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    /// ZeroClaw agent alias (`[agents.<alias>]`), published over A2A.
    pub agent: String,
    /// Upstream model id as it appears in the cost ledger.
    pub model: String,
    /// Model vendor. The verifier must come from a different vendor than the
    /// writer, because a model checking its own family's output shares its
    /// blind spots.
    pub vendor: String,
}

/// Which pipeline role a model call serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Analyst,
    Writer,
    Verifier,
}

impl Roles {
    #[must_use]
    pub fn binding(&self, role: Role) -> &RoleBinding {
        match role {
            Role::Analyst => &self.analyst,
            Role::Writer => &self.writer,
            Role::Verifier => &self.verifier,
        }
    }

    /// Distinct models the roles use, for budget reporting.
    #[must_use]
    pub fn models(&self) -> BTreeSet<&str> {
        [&self.analyst, &self.writer, &self.verifier]
            .into_iter()
            .map(|b| b.model.as_str())
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetPolicy {
    /// Hard monthly cap per model, in USD, over the UTC calendar month.
    #[serde(default = "default_monthly_cap")]
    pub monthly_cap_usd_per_model: f64,
    /// Headroom a model must have before one more call is made. Set it to
    /// the cost of your most expensive single call.
    #[serde(default = "default_reserve")]
    pub reserve_usd_per_call: f64,
}

impl Default for BudgetPolicy {
    fn default() -> Self {
        Self {
            monthly_cap_usd_per_model: default_monthly_cap(),
            reserve_usd_per_call: default_reserve(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchPolicy {
    #[serde(default = "default_fetch_timeout_secs")]
    pub timeout_secs: u64,
    /// Largest response body accepted from a source.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    /// Most characters of changed text sent to the models per run. Bounds
    /// cost when a source changes wholesale.
    #[serde(default = "default_max_evidence_chars")]
    pub max_evidence_chars: usize,
}

impl Default for FetchPolicy {
    fn default() -> Self {
        Self {
            timeout_secs: default_fetch_timeout_secs(),
            max_bytes: default_max_bytes(),
            user_agent: default_user_agent(),
            max_evidence_chars: default_max_evidence_chars(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub id: String,
    /// Pack id.
    pub pack: String,
    pub delivery: DeliveryMode,
    pub destination: Destination,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Destination {
    /// Write each delivered brief as a JSON file, for a reseller or another
    /// system to pick up.
    Outbox { dir: PathBuf },
    /// POST each delivered brief as JSON to an HTTPS endpoint.
    Webhook {
        url: Url,
        /// Environment variable holding a bearer token, if the endpoint
        /// needs one.
        #[serde(default)]
        token_env: Option<String>,
    },
}

fn default_agent_timeout_secs() -> u64 {
    180
}
fn default_monthly_cap() -> f64 {
    20.0
}
fn default_reserve() -> f64 {
    0.25
}
fn default_fetch_timeout_secs() -> u64 {
    20
}
fn default_max_bytes() -> usize {
    2_000_000
}
fn default_user_agent() -> String {
    format!("brief-harness/{}", env!("CARGO_PKG_VERSION"))
}
fn default_max_evidence_chars() -> usize {
    24_000
}

/// ZeroClaw alias rule: lowercase ASCII letters and digits with single
/// underscores, no leading or trailing underscore.
#[must_use]
pub fn is_agent_alias(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_env_var_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with(|c: char| c.is_ascii_digit())
        && value
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
}

fn is_loopback_host(url: &Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

impl HarnessConfig {
    /// Load, resolve relative paths against the config file's directory, and
    /// validate.
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut config: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        config.resolve_paths(base);
        config.validate()?;
        Ok(config)
    }

    fn resolve_paths(&mut self, base: &Path) {
        let resolve = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        resolve(&mut self.state_dir);
        resolve(&mut self.packs_dir);
        resolve(&mut self.zeroclaw.cost_ledger);
        for sub in &mut self.subscriptions {
            if let Destination::Outbox { dir } = &mut sub.destination {
                resolve(dir);
            }
        }
    }

    /// Check every invariant that does not need the packs loaded.
    pub fn validate(&self) -> Result<()> {
        let gateway = &self.zeroclaw.gateway_url;
        match gateway.scheme() {
            "https" => {}
            "http" if is_loopback_host(gateway) => {}
            "http" => bail!(
                "zeroclaw.gateway_url must use https unless it is a loopback address, \
                 because the bearer token travels with every call"
            ),
            other => bail!("zeroclaw.gateway_url has unsupported scheme {other}"),
        }
        if !is_env_var_name(&self.zeroclaw.token_env) {
            bail!("zeroclaw.token_env must be an environment variable name");
        }
        if !(1..=900).contains(&self.zeroclaw.agent_timeout_secs) {
            bail!("zeroclaw.agent_timeout_secs must be between 1 and 900");
        }

        for (name, binding) in [
            ("analyst", &self.roles.analyst),
            ("writer", &self.roles.writer),
            ("verifier", &self.roles.verifier),
        ] {
            if !is_agent_alias(&binding.agent) {
                bail!(
                    "roles.{name}.agent {:?} is not a valid ZeroClaw alias",
                    binding.agent
                );
            }
            if binding.model.trim().is_empty() || binding.vendor.trim().is_empty() {
                bail!("roles.{name} needs both model and vendor");
            }
        }
        if self
            .roles
            .verifier
            .vendor
            .eq_ignore_ascii_case(&self.roles.writer.vendor)
        {
            bail!(
                "roles.verifier must use a different vendor than roles.writer; same-family \
                 review shares the writer's blind spots"
            );
        }
        if self.roles.verifier.agent == self.roles.writer.agent {
            bail!("roles.verifier and roles.writer must be different agents");
        }

        let cap = self.budget.monthly_cap_usd_per_model;
        let reserve = self.budget.reserve_usd_per_call;
        if !cap.is_finite() || cap <= 0.0 {
            bail!("budget.monthly_cap_usd_per_model must be a positive number");
        }
        if !reserve.is_finite() || reserve <= 0.0 || reserve >= cap {
            bail!("budget.reserve_usd_per_call must be positive and below the monthly cap");
        }

        if !(1..=120).contains(&self.fetch.timeout_secs) {
            bail!("fetch.timeout_secs must be between 1 and 120");
        }
        if !(1_024..=20_000_000).contains(&self.fetch.max_bytes) {
            bail!("fetch.max_bytes must be between 1 KiB and 20 MB");
        }
        if !(1_000..=200_000).contains(&self.fetch.max_evidence_chars) {
            bail!("fetch.max_evidence_chars must be between 1,000 and 200,000");
        }

        let mut ids = BTreeSet::new();
        for sub in &self.subscriptions {
            if !is_slug(&sub.id) {
                bail!("subscription id {:?} must be a lowercase slug", sub.id);
            }
            if !ids.insert(sub.id.as_str()) {
                bail!("duplicate subscription id {}", sub.id);
            }
            if let Destination::Webhook { url, token_env } = &sub.destination {
                if url.scheme() != "https" {
                    bail!("subscription {}: webhook url must use https", sub.id);
                }
                if token_env.as_deref().is_some_and(|n| !is_env_var_name(n)) {
                    bail!(
                        "subscription {}: token_env must be an environment variable name",
                        sub.id
                    );
                }
            }
        }
        Ok(())
    }

    /// Check that every subscription names a loaded pack.
    pub fn validate_against_packs(
        &self,
        packs: &std::collections::BTreeMap<String, VerticalPack>,
    ) -> Result<()> {
        for sub in &self.subscriptions {
            if !packs.contains_key(&sub.pack) {
                bail!(
                    "subscription {} names pack {}, which is not in {}",
                    sub.id,
                    sub.pack,
                    self.packs_dir.display()
                );
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn subscription(&self, id: &str) -> Option<&Subscription> {
        self.subscriptions.iter().find(|s| s.id == id)
    }

    /// The kill switch: while this file exists, nothing is fetched,
    /// generated, or sent.
    #[must_use]
    pub fn kill_switch_path(&self) -> PathBuf {
        self.state_dir.join("KILL")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn sample_toml() -> String {
        r#"
state_dir = "state"
packs_dir = "packs"

[zeroclaw]
gateway_url = "http://127.0.0.1:42617"
token_env = "BRIEF_HARNESS_ZEROCLAW_TOKEN"
cost_ledger = "/srv/zeroclaw/workspace/state/costs.jsonl"

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
id = "acme-rfp"
pack = "rfp-scouting"
delivery = "approval"
destination = { kind = "outbox", dir = "outbox" }
"#
        .to_string()
    }

    fn load_str(text: &str) -> Result<HarnessConfig> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("brief-harness.toml");
        fs::write(&path, text)?;
        HarnessConfig::load(&path)
    }

    #[test]
    fn sample_loads_with_defaults_and_resolved_paths() {
        let config = load_str(&sample_toml()).unwrap();
        assert!((config.budget.monthly_cap_usd_per_model - 20.0).abs() < f64::EPSILON);
        assert!(config.state_dir.is_absolute());
        assert_eq!(config.subscriptions[0].delivery, DeliveryMode::Approval);
        assert_eq!(config.roles.models().len(), 3);
        let Destination::Outbox { dir } = &config.subscriptions[0].destination else {
            panic!("expected outbox");
        };
        assert!(dir.is_absolute());
    }

    #[test]
    fn verifier_must_be_another_vendor() {
        let text = sample_toml().replace("vendor = \"openai\"", "vendor = \"Anthropic\"");
        let err = load_str(&text).unwrap_err().to_string();
        assert!(err.contains("different vendor"), "{err}");
    }

    #[test]
    fn remote_plain_http_gateway_is_refused() {
        let text = sample_toml().replace("http://127.0.0.1:42617", "http://10.0.0.5:42617");
        assert!(load_str(&text).is_err());
        let ok = sample_toml().replace("http://127.0.0.1:42617", "https://zc.example.net");
        assert!(load_str(&ok).is_ok());
        let v6 = sample_toml().replace("http://127.0.0.1:42617", "http://[::1]:42617");
        assert!(load_str(&v6).is_ok());
    }

    #[test]
    fn budget_bounds_are_enforced() {
        let zero_cap = format!(
            "{}\n[budget]\nmonthly_cap_usd_per_model = 0.0\n",
            sample_toml()
        );
        assert!(load_str(&zero_cap).is_err());
        let big_reserve = format!(
            "{}\n[budget]\nmonthly_cap_usd_per_model = 20.0\nreserve_usd_per_call = 25.0\n",
            sample_toml()
        );
        assert!(load_str(&big_reserve).is_err());
    }

    #[test]
    fn webhook_requires_https() {
        let text = sample_toml().replace(
            "destination = { kind = \"outbox\", dir = \"outbox\" }",
            "destination = { kind = \"webhook\", url = \"http://hooks.example.com/x\" }",
        );
        assert!(load_str(&text).is_err());
    }

    #[test]
    fn unknown_delivery_mode_is_rejected() {
        let text = sample_toml().replace("delivery = \"approval\"", "delivery = \"yolo\"");
        assert!(load_str(&text).is_err());
    }

    #[test]
    fn alias_rules_match_zeroclaw() {
        assert!(is_agent_alias("brief_writer"));
        for bad in ["brief-writer", "_a", "a_", "a__b", "A"] {
            assert!(!is_agent_alias(bad), "{bad:?}");
        }
    }
}
