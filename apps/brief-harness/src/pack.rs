//! Vertical packs: the unit that makes the harness vertical-agnostic.
//!
//! A pack is one TOML file naming the sources a vertical watches and the
//! instructions its analyst and writer follow. The pack's source list is the
//! fetch allowlist: the harness requests exactly these URLs and nothing else,
//! and refuses redirects, so a pack cannot be steered to another host.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use url::Url;

/// Upper bound on sources per pack, so one pack cannot turn a run into a
/// crawl.
pub const MAX_SOURCES: usize = 100;
/// Upper bound on brief items a pack may request.
pub const MAX_ITEMS_LIMIT: usize = 20;
/// Upper bound on pack-supplied instruction text, in characters.
pub const MAX_INSTRUCTION_CHARS: usize = 4_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerticalPack {
    /// Stable slug; must match the file stem.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Most items a single brief may contain.
    #[serde(default = "default_max_items")]
    pub max_items: usize,
    pub analyst: RoleInstructions,
    pub writer: RoleInstructions,
    pub sources: Vec<SourceSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleInstructions {
    /// Vertical-specific guidance: what matters to this buyer and what is
    /// noise. The harness wraps it in a fixed output contract.
    pub instructions: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// Stable slug, unique within the pack. Citations name sources by it.
    pub id: String,
    pub label: String,
    pub url: Url,
}

fn default_max_items() -> usize {
    5
}

/// True when `value` is a lowercase ASCII slug: `a-z`, `0-9`, single
/// hyphens, no leading or trailing hyphen, at most 64 characters.
#[must_use]
pub fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

impl VerticalPack {
    /// Parse and validate a pack from TOML text.
    pub fn from_toml(text: &str) -> Result<Self> {
        let pack: Self = toml::from_str(text).context("parsing pack TOML")?;
        pack.validate()?;
        Ok(pack)
    }

    /// Enforce the invariants the rest of the pipeline relies on.
    pub fn validate(&self) -> Result<()> {
        if !is_slug(&self.id) {
            bail!("pack id {:?} must be a lowercase slug", self.id);
        }
        if self.name.trim().is_empty() {
            bail!("pack {}: name must not be empty", self.id);
        }
        if self.max_items == 0 || self.max_items > MAX_ITEMS_LIMIT {
            bail!(
                "pack {}: max_items must be between 1 and {MAX_ITEMS_LIMIT}",
                self.id
            );
        }
        for (role, text) in [
            ("analyst", &self.analyst.instructions),
            ("writer", &self.writer.instructions),
        ] {
            if text.trim().is_empty() {
                bail!("pack {}: {role}.instructions must not be empty", self.id);
            }
            if text.chars().count() > MAX_INSTRUCTION_CHARS {
                bail!(
                    "pack {}: {role}.instructions exceeds {MAX_INSTRUCTION_CHARS} characters",
                    self.id
                );
            }
        }
        if self.sources.is_empty() {
            bail!("pack {}: at least one source is required", self.id);
        }
        if self.sources.len() > MAX_SOURCES {
            bail!("pack {}: at most {MAX_SOURCES} sources", self.id);
        }
        let mut ids = BTreeSet::new();
        let mut urls = BTreeSet::new();
        for source in &self.sources {
            if !is_slug(&source.id) {
                bail!(
                    "pack {}: source id {:?} must be a lowercase slug",
                    self.id,
                    source.id
                );
            }
            if !ids.insert(source.id.as_str()) {
                bail!("pack {}: duplicate source id {}", self.id, source.id);
            }
            validate_source_url(&self.id, source)?;
            if !urls.insert(source.url.as_str()) {
                bail!("pack {}: duplicate source url {}", self.id, source.url);
            }
        }
        Ok(())
    }

    /// Look up a source by id.
    #[must_use]
    pub fn source(&self, id: &str) -> Option<&SourceSpec> {
        self.sources.iter().find(|s| s.id == id)
    }
}

fn validate_source_url(pack_id: &str, source: &SourceSpec) -> Result<()> {
    let url = &source.url;
    if url.scheme() != "https" {
        bail!(
            "pack {pack_id}: source {} must use https, got {}",
            source.id,
            url.scheme()
        );
    }
    if url.host_str().is_none_or(str::is_empty) {
        bail!("pack {pack_id}: source {} has no host", source.id);
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "pack {pack_id}: source {} must not embed credentials in its URL",
            source.id
        );
    }
    if url.fragment().is_some() {
        bail!(
            "pack {pack_id}: source {} must not carry a #fragment",
            source.id
        );
    }
    Ok(())
}

/// Load one pack file and check that its id matches the file stem.
pub fn load_pack(path: &Path) -> Result<VerticalPack> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let pack =
        VerticalPack::from_toml(&text).with_context(|| format!("loading {}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if stem != pack.id {
        bail!(
            "{}: pack id {} must match the file name",
            path.display(),
            pack.id
        );
    }
    Ok(pack)
}

/// Load every `*.toml` pack in `dir`, keyed by pack id.
pub fn load_packs_dir(dir: &Path) -> Result<BTreeMap<String, VerticalPack>> {
    let mut packs = BTreeMap::new();
    let entries = fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            paths.push(path);
        }
    }
    paths.sort();
    for path in paths {
        let pack = load_pack(&path)?;
        packs.insert(pack.id.clone(), pack);
    }
    Ok(packs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
id = "rfp-scouting"
name = "RFP and grant scouting"
description = "Public procurement notices for small IT firms."
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

    #[test]
    fn valid_pack_parses() {
        let pack = VerticalPack::from_toml(VALID).unwrap();
        assert_eq!(pack.id, "rfp-scouting");
        assert_eq!(pack.max_items, 3);
        assert_eq!(pack.sources.len(), 2);
        assert!(pack.source("city-bids").is_some());
    }

    #[test]
    fn slug_rules() {
        assert!(is_slug("msp-advisories"));
        assert!(is_slug("a1"));
        for bad in ["", "-a", "a-", "a--b", "A", "a_b", "a b"] {
            assert!(!is_slug(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn rejects_plain_http_source() {
        let text = VALID.replace("https://bids.example.gov", "http://bids.example.gov");
        let err = VerticalPack::from_toml(&text).unwrap_err().to_string();
        assert!(err.contains("https"), "{err}");
    }

    #[test]
    fn rejects_credentials_in_url() {
        let text = VALID.replace("https://bids.", "https://user:pw@bids.");
        assert!(VerticalPack::from_toml(&text).is_err());
    }

    #[test]
    fn rejects_duplicate_source_ids_and_urls() {
        let dup_id = VALID.replace("state-grants", "city-bids");
        assert!(VerticalPack::from_toml(&dup_id).is_err());
        let dup_url = VALID.replace(
            "https://grants.example.gov/list",
            "https://bids.example.gov/open",
        );
        assert!(VerticalPack::from_toml(&dup_url).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = VALID.replace("max_items = 3", "max_items = 3\nauto_send = true");
        assert!(VerticalPack::from_toml(&text).is_err());
    }

    #[test]
    fn rejects_out_of_range_max_items() {
        assert!(VerticalPack::from_toml(&VALID.replace("max_items = 3", "max_items = 0")).is_err());
        assert!(
            VerticalPack::from_toml(&VALID.replace("max_items = 3", "max_items = 21")).is_err()
        );
    }

    #[test]
    fn load_pack_requires_matching_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("rfp-scouting.toml");
        fs::write(&good, VALID).unwrap();
        assert!(load_pack(&good).is_ok());
        let bad = dir.path().join("other.toml");
        fs::write(&bad, VALID).unwrap();
        assert!(load_pack(&bad).is_err());
    }
}
