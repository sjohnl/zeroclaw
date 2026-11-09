//! Deterministic collection and change detection. No model is involved.
//!
//! Each source is fetched, reduced to normalized text, and stored
//! content-addressed by SHA-256. A run compares the new hash with the
//! source's last one; only a changed source produces work for the models,
//! so a quiet week costs nothing.

use std::fs;
use std::future::Future;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use similar::{ChangeTag, TextDiff};

use crate::config::FetchPolicy;
use crate::fsutil::{sha256_hex, write_atomic, write_json_atomic};
use crate::pack::SourceSpec;

/// A fetched response body.
#[derive(Debug, Clone)]
pub struct FetchedDoc {
    pub body: String,
    pub content_type: Option<String>,
}

/// Retrieves one source. The production implementation is [`HttpFetcher`];
/// tests substitute fixtures.
pub trait Fetcher {
    fn fetch(&self, source: &SourceSpec) -> impl Future<Output = Result<FetchedDoc>> + Send;
}

/// HTTPS fetcher with a timeout, a body-size cap, and redirects refused, so
/// a source can never hand the harness a URL outside its pack.
#[derive(Debug, Clone)]
pub struct HttpFetcher {
    client: reqwest::Client,
    max_bytes: usize,
}

impl HttpFetcher {
    pub fn new(policy: &FetchPolicy) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(policy.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(policy.user_agent.clone())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            max_bytes: policy.max_bytes,
        })
    }
}

impl Fetcher for HttpFetcher {
    async fn fetch(&self, source: &SourceSpec) -> Result<FetchedDoc> {
        let mut response = self
            .client
            .get(source.url.clone())
            .send()
            .await
            .with_context(|| format!("fetching {}", source.url))?;
        let status = response.status();
        if !status.is_success() {
            bail!("{} answered {status}", source.url);
        }
        if response
            .content_length()
            .is_some_and(|len| len > self.max_bytes as u64)
        {
            bail!("{} is larger than {} bytes", source.url, self.max_bytes);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .with_context(|| format!("reading {}", source.url))?
        {
            if body.len() + chunk.len() > self.max_bytes {
                bail!("{} is larger than {} bytes", source.url, self.max_bytes);
            }
            body.extend_from_slice(&chunk);
        }
        Ok(FetchedDoc {
            body: String::from_utf8_lossy(&body).into_owned(),
            content_type,
        })
    }
}

/// Collapse every run of whitespace to one space and trim.
#[must_use]
pub fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Reduce a response to the text that is hashed, diffed, and quoted:
/// markup stripped from HTML and XML, JSON re-printed one field per line
/// (so a minified feed still diffs entry by entry), whitespace collapsed
/// per line, blank lines dropped.
#[must_use]
pub fn normalize(doc: &FetchedDoc) -> String {
    let content_type = doc.content_type.as_deref().unwrap_or_default();
    let trimmed = doc.body.trim_start();
    let is_json = content_type.contains("json") || trimmed.starts_with(['{', '[']);
    let is_markup =
        content_type.contains("html") || content_type.contains("xml") || trimmed.starts_with('<');
    let pretty_json = is_json
        .then(|| serde_json::from_str::<serde_json::Value>(&doc.body).ok())
        .flatten()
        .and_then(|value| serde_json::to_string_pretty(&value).ok());
    let text = match pretty_json {
        Some(json) => json,
        None if is_markup => nanohtml2text::html2text(&doc.body),
        None => doc.body.clone(),
    };
    text.lines()
        .map(collapse_whitespace)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Metadata for one stored snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// Subscription whose change history this snapshot belongs to. Each
    /// subscription keeps its own "last seen" pointer, so two subscriptions
    /// on one pack both see every change.
    pub subscription: String,
    pub source_id: String,
    pub url: String,
    pub sha256: String,
    pub captured_at: DateTime<Utc>,
    pub chars: usize,
}

/// A source whose normalized text changed since the last run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub previous: SnapshotMeta,
    pub current: SnapshotMeta,
    /// Lines present now and not before.
    pub added: Vec<String>,
    /// Lines present before and not now.
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// First capture of this source; nothing to compare with.
    Baseline(SnapshotMeta),
    Unchanged(SnapshotMeta),
    Changed(Change),
}

/// True when `value` is a lowercase 64-character hex digest. Snapshot ids
/// arrive in model output, so they are checked before touching the disk.
#[must_use]
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Content-addressed snapshot storage under `<state>/snapshots`.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn blob_path(&self, sha: &str) -> PathBuf {
        self.root
            .join("blobs")
            .join(&sha[..2])
            .join(format!("{sha}.txt"))
    }

    fn latest_path(&self, subscription: &str, source_id: &str) -> PathBuf {
        self.root
            .join("latest")
            .join(subscription)
            .join(format!("{source_id}.json"))
    }

    /// The last snapshot a subscription recorded for a source, if any.
    pub fn latest(&self, subscription: &str, source_id: &str) -> Result<Option<SnapshotMeta>> {
        let path = self.latest_path(subscription, source_id);
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .with_context(|| format!("parsing {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Read a stored snapshot's text, checking that it still hashes to its
    /// name so tampered evidence is never quoted.
    pub fn read_text(&self, sha: &str) -> Result<String> {
        if !is_sha256_hex(sha) {
            bail!("{sha:?} is not a snapshot id");
        }
        let path = self.blob_path(sha);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        if sha256_hex(text.as_bytes()) != sha {
            bail!("snapshot {sha} no longer matches its hash");
        }
        Ok(text)
    }

    /// Store normalized `text` for `source` and classify it against the
    /// subscription's previous snapshot. Blobs are shared; pointers are not.
    pub fn record(
        &self,
        subscription: &str,
        source: &SourceSpec,
        text: &str,
        captured_at: DateTime<Utc>,
    ) -> Result<ChangeOutcome> {
        let sha = sha256_hex(text.as_bytes());
        let blob = self.blob_path(&sha);
        if !blob.exists() {
            write_atomic(&blob, text.as_bytes())?;
        }
        let current = SnapshotMeta {
            subscription: subscription.to_owned(),
            source_id: source.id.clone(),
            url: source.url.to_string(),
            sha256: sha,
            captured_at,
            chars: text.chars().count(),
        };
        let previous = self.latest(subscription, &source.id)?;
        let outcome = match previous {
            Some(prev) if prev.sha256 == current.sha256 => {
                return Ok(ChangeOutcome::Unchanged(prev));
            }
            Some(prev) => match self.read_text(&prev.sha256) {
                Ok(prev_text) => {
                    let (added, removed) = line_diff(&prev_text, text);
                    ChangeOutcome::Changed(Change {
                        previous: prev,
                        current: current.clone(),
                        added,
                        removed,
                    })
                }
                // The previous blob is gone or damaged: start a fresh
                // baseline rather than diffing against unknown text.
                Err(_) => ChangeOutcome::Baseline(current.clone()),
            },
            None => ChangeOutcome::Baseline(current.clone()),
        };
        write_json_atomic(&self.latest_path(subscription, &source.id), &current)?;
        Ok(outcome)
    }
}

/// Lines added and removed between two normalized texts.
#[must_use]
pub fn line_diff(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    let diff = TextDiff::from_lines(old, new);
    let mut added = Vec::new();
    let mut removed = Vec::new();
    for change in diff.iter_all_changes() {
        let line = change.value().trim_end_matches('\n').to_owned();
        if line.is_empty() {
            continue;
        }
        match change.tag() {
            ChangeTag::Insert => added.push(line),
            ChangeTag::Delete => removed.push(line),
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn source(url: &str) -> SourceSpec {
        SourceSpec {
            id: "city-bids".into(),
            label: "City bids".into(),
            url: url.parse().unwrap(),
        }
    }

    #[test]
    fn normalize_strips_markup_and_whitespace() {
        let doc = FetchedDoc {
            body: "<html><body><h1>Open  bids</h1>\n<p>Network   refresh RFP</p>\n\n</body></html>"
                .into(),
            content_type: Some("text/html; charset=utf-8".into()),
        };
        let text = normalize(&doc);
        assert!(text.contains("Open bids"), "{text}");
        assert!(text.contains("Network refresh RFP"), "{text}");
        assert!(!text.contains('<'));
        assert!(!text.lines().any(|l| l.trim().is_empty()));
    }

    #[test]
    fn minified_json_diffs_entry_by_entry() {
        let old = FetchedDoc {
            body: r#"{"vulnerabilities":[{"cveID":"CVE-2026-0001"}]}"#.into(),
            content_type: Some("application/json".into()),
        };
        let new = FetchedDoc {
            body: r#"{"vulnerabilities":[{"cveID":"CVE-2026-0001"},{"cveID":"CVE-2026-0002"}]}"#
                .into(),
            content_type: Some("application/json".into()),
        };
        let (added, _) = line_diff(&normalize(&old), &normalize(&new));
        assert!(
            added.iter().any(|l| l == "\"cveID\": \"CVE-2026-0002\""),
            "{added:?}"
        );
        let broken = FetchedDoc {
            body: "{not json".into(),
            content_type: Some("application/json".into()),
        };
        assert_eq!(
            normalize(&broken),
            "{not json",
            "unparseable JSON stays as text"
        );
    }

    #[test]
    fn plain_text_is_kept_line_by_line() {
        let doc = FetchedDoc {
            body: "a  b\n\n c ".into(),
            content_type: Some("text/plain".into()),
        };
        assert_eq!(normalize(&doc), "a b\nc");
    }

    #[test]
    fn baseline_then_unchanged_then_changed() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        let src = source("https://bids.example.gov/open");
        let t0 = Utc::now();

        let first = store.record("p", &src, "bid A\nbid B", t0).unwrap();
        assert!(matches!(first, ChangeOutcome::Baseline(_)));

        let same = store.record("p", &src, "bid A\nbid B", t0).unwrap();
        assert!(matches!(same, ChangeOutcome::Unchanged(_)));

        let ChangeOutcome::Changed(change) = store.record("p", &src, "bid A\nbid C", t0).unwrap()
        else {
            panic!("expected a change");
        };
        assert_eq!(change.added, vec!["bid C"]);
        assert_eq!(change.removed, vec!["bid B"]);
        assert_ne!(change.previous.sha256, change.current.sha256);
        assert_eq!(
            store.read_text(&change.current.sha256).unwrap(),
            "bid A\nbid C"
        );
        assert_eq!(
            store.latest("p", "city-bids").unwrap().unwrap().sha256,
            change.current.sha256
        );
    }

    #[test]
    fn subscriptions_track_changes_independently() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        let src = source("https://bids.example.gov/open");
        let t0 = Utc::now();
        store.record("sub-a", &src, "v1", t0).unwrap();
        store.record("sub-b", &src, "v1", t0).unwrap();
        assert!(matches!(
            store.record("sub-a", &src, "v2", t0).unwrap(),
            ChangeOutcome::Changed(_)
        ));
        assert!(
            matches!(
                store.record("sub-b", &src, "v2", t0).unwrap(),
                ChangeOutcome::Changed(_)
            ),
            "sub-a seeing the change first must not hide it from sub-b"
        );
    }

    #[test]
    fn read_text_rejects_bad_ids_and_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(dir.path().to_path_buf());
        assert!(store.read_text("../../etc/passwd").is_err());
        let src = source("https://bids.example.gov/open");
        let ChangeOutcome::Baseline(meta) =
            store.record("p", &src, "original", Utc::now()).unwrap()
        else {
            panic!("expected baseline");
        };
        fs::write(store.blob_path(&meta.sha256), "edited").unwrap();
        assert!(store.read_text(&meta.sha256).is_err());
    }

    #[tokio::test]
    async fn http_fetcher_reads_body_and_refuses_redirects_and_oversize() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("<p>hello</p>", "text/html"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/moved"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "https://elsewhere.example/"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(4096)))
            .mount(&server)
            .await;

        let policy = FetchPolicy {
            max_bytes: 1024,
            ..FetchPolicy::default()
        };
        let fetcher = HttpFetcher::new(&policy).unwrap();
        let doc = fetcher
            .fetch(&source(&format!("{}/ok", server.uri())))
            .await
            .unwrap();
        assert_eq!(doc.body, "<p>hello</p>");
        assert_eq!(doc.content_type.as_deref(), Some("text/html"));

        assert!(
            fetcher
                .fetch(&source(&format!("{}/moved", server.uri())))
                .await
                .is_err(),
            "a redirect must fail, not be followed"
        );
        assert!(
            fetcher
                .fetch(&source(&format!("{}/big", server.uri())))
                .await
                .is_err(),
            "an oversized body must fail"
        );
    }
}
