//! The shipped samples must keep loading as the schema evolves.

use std::path::Path;

use brief_harness::config::HarnessConfig;
use brief_harness::pack::load_packs_dir;

fn samples() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/samples"))
}

#[test]
fn sample_packs_validate() {
    let packs = load_packs_dir(&samples().join("packs")).unwrap();
    let ids: Vec<_> = packs.keys().map(String::as_str).collect();
    assert_eq!(ids, ["agency-radar", "msp-advisories", "rfp-scouting"]);
}

#[test]
fn sample_config_validates_against_sample_packs() {
    let config = HarnessConfig::load(&samples().join("brief-harness.toml")).unwrap();
    let packs = load_packs_dir(&config.packs_dir).unwrap();
    config.validate_against_packs(&packs).unwrap();
    assert_eq!(config.subscriptions.len(), 3);
}

/// Parse the fragment with ZeroClaw's own config schema and check the
/// values that keep the role agents inert landed where the schema reads
/// them, so a misspelled key cannot pass silently.
#[test]
fn sample_zeroclaw_fragment_matches_the_daemon_schema() {
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::schema::Config;

    let text = std::fs::read_to_string(samples().join("zeroclaw-agents.toml")).unwrap();
    let config: Config = toml::from_str(&text).unwrap();

    assert!(config.a2a.server.enabled);
    let risk = &config.risk_profiles["brief_role"];
    assert_eq!(risk.level, AutonomyLevel::ReadOnly);
    assert_eq!(
        risk.effective_allowed_tools(),
        Some(Vec::new()),
        "deny-all tools"
    );
    let runtime = &config.runtime_profiles["brief_role"];
    assert!(!runtime.agentic);
    assert_eq!(runtime.max_cost_per_day_cents, 200);
    assert_eq!(config.cost.enforcement.mode, "block");
    assert!((config.cost.monthly_limit_usd - 60.0).abs() < f64::EPSILON);
    for agent in ["brief_analyst", "brief_writer", "brief_verifier"] {
        let entry = &config.agents[agent];
        assert!(entry.a2a.published, "{agent} must be published over A2A");
    }
}
