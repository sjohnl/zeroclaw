//! Append-only, hash-chained audit log (`<state>/audit.jsonl`).
//!
//! ZeroClaw records approvals and costs but not every action an autonomous
//! run takes. This log closes that gap for the harness: each entry carries
//! the SHA-256 of the entry before it, so editing, deleting, or reordering
//! any entry breaks verification from that point on. Entries record hashes
//! of prompts and replies, not their text, so the log holds no source or
//! subscriber content.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Role;
use crate::delivery::DeliveryMode;
use crate::fsutil::sha256_hex;

/// `prev` value of the first entry.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AuditEvent {
    RunStarted {
        subscription: String,
        pack: String,
        delivery: DeliveryMode,
    },
    KillSwitchEngaged,
    SourceBaseline {
        source_id: String,
        snapshot: String,
    },
    SourceUnchanged {
        source_id: String,
        snapshot: String,
    },
    SourceChanged {
        source_id: String,
        previous: String,
        current: String,
        added_lines: usize,
        removed_lines: usize,
    },
    SourceFailed {
        source_id: String,
        error: String,
    },
    BudgetChecked {
        role: Role,
        model: String,
        allowed: bool,
        detail: String,
    },
    AgentCalled {
        role: Role,
        agent: String,
        model: String,
        prompt_sha256: String,
        reply_sha256: String,
    },
    AgentFailed {
        role: Role,
        agent: String,
        error: String,
    },
    Triaged {
        relevant_changes: Vec<usize>,
    },
    BriefRejected {
        reason: String,
    },
    CitationCheck {
        passed: bool,
        failures: Vec<String>,
    },
    SemanticCheck {
        passed: bool,
        unsupported_items: Vec<usize>,
    },
    DeliveryDecided {
        decision: String,
        reasons: Vec<String>,
    },
    Delivered {
        receipt: String,
        brief_sha256: String,
    },
    DeliveryFailed {
        error: String,
    },
    QueuedForApproval {
        approval_id: String,
    },
    Held {
        path: String,
    },
    ApprovalResolved {
        approval_id: String,
        approved: bool,
        by: String,
        reason: Option<String>,
    },
    RunFinished {
        status: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub run_id: String,
    pub event: AuditEvent,
    pub prev: String,
    pub hash: String,
}

/// The hashed portion of an entry: everything except `hash` itself.
#[derive(Serialize)]
struct Unsealed<'a> {
    seq: u64,
    at: &'a DateTime<Utc>,
    run_id: &'a str,
    event: &'a AuditEvent,
    prev: &'a str,
}

fn seal(
    seq: u64,
    at: &DateTime<Utc>,
    run_id: &str,
    event: &AuditEvent,
    prev: &str,
) -> Result<String> {
    let bytes = serde_json::to_vec(&Unsealed {
        seq,
        at,
        run_id,
        event,
        prev,
    })
    .context("serializing audit entry")?;
    Ok(sha256_hex(&bytes))
}

/// Result of a successful chain verification.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ChainSummary {
    pub entries: u64,
    pub head: String,
}

/// Verify the whole chain in `path`. An absent file is an empty, valid
/// chain.
pub fn verify(path: &Path) -> Result<ChainSummary> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ChainSummary {
                entries: 0,
                head: GENESIS.to_owned(),
            });
        }
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let mut expected_prev = GENESIS.to_owned();
    let mut count = 0_u64;
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            bail!("audit line {line_no} is blank");
        }
        let entry: AuditEntry = serde_json::from_str(line)
            .with_context(|| format!("audit line {line_no} is not a valid entry"))?;
        if entry.seq != count {
            bail!(
                "audit line {line_no}: sequence {} where {count} was expected",
                entry.seq
            );
        }
        if entry.prev != expected_prev {
            bail!("audit line {line_no}: chain broken, prev hash does not match line before");
        }
        let recomputed = seal(
            entry.seq,
            &entry.at,
            &entry.run_id,
            &entry.event,
            &entry.prev,
        )?;
        if recomputed != entry.hash {
            bail!("audit line {line_no}: contents do not match their hash");
        }
        expected_prev = entry.hash;
        count += 1;
    }
    Ok(ChainSummary {
        entries: count,
        head: expected_prev,
    })
}

/// Writer for the audit chain. Opening verifies the existing chain, so a
/// run never extends a log that has been tampered with.
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    next_seq: u64,
    prev: String,
}

impl AuditLog {
    pub fn open(path: &Path) -> Result<Self> {
        let summary = verify(path).with_context(|| {
            format!(
                "refusing to append to {}: its hash chain does not verify",
                path.display()
            )
        })?;
        Ok(Self {
            path: path.to_path_buf(),
            next_seq: summary.entries,
            prev: summary.head,
        })
    }

    /// Append one event and flush it to disk before returning.
    pub fn append(&mut self, run_id: &str, event: AuditEvent) -> Result<AuditEntry> {
        let at = Utc::now();
        let hash = seal(self.next_seq, &at, run_id, &event, &self.prev)?;
        let entry = AuditEntry {
            seq: self.next_seq,
            at,
            run_id: run_id.to_owned(),
            event,
            prev: self.prev.clone(),
            hash,
        };
        let mut line = serde_json::to_string(&entry).context("serializing audit entry")?;
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", self.path.display()))?;
        file.sync_data()
            .with_context(|| format!("syncing {}", self.path.display()))?;
        self.prev.clone_from(&entry.hash);
        self.next_seq += 1;
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(log: &mut AuditLog) {
        log.append("run-1", AuditEvent::KillSwitchEngaged).unwrap();
        log.append(
            "run-1",
            AuditEvent::SourceFailed {
                source_id: "city-bids".into(),
                error: "timeout".into(),
            },
        )
        .unwrap();
        log.append(
            "run-1",
            AuditEvent::RunFinished {
                status: "kill_switch".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn appended_chain_verifies_and_reopen_continues_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).unwrap();
        sample(&mut log);
        assert_eq!(verify(&path).unwrap().entries, 3);

        let mut reopened = AuditLog::open(&path).unwrap();
        let entry = reopened
            .append("run-2", AuditEvent::KillSwitchEngaged)
            .unwrap();
        assert_eq!(entry.seq, 3);
        assert_eq!(verify(&path).unwrap().entries, 4);
    }

    #[test]
    fn edited_entry_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).unwrap();
        sample(&mut log);
        let text = fs::read_to_string(&path).unwrap().replace("timeout", "ok");
        fs::write(&path, text).unwrap();
        let err = verify(&path).unwrap_err().to_string();
        assert!(err.contains("line 2"), "{err}");
        assert!(
            AuditLog::open(&path).is_err(),
            "must not extend a broken chain"
        );
    }

    #[test]
    fn deleted_entry_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).unwrap();
        sample(&mut log);
        let lines: Vec<_> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        fs::write(&path, format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        assert!(verify(&path).is_err());
    }

    #[test]
    fn injected_field_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let mut log = AuditLog::open(&path).unwrap();
        sample(&mut log);
        let text =
            fs::read_to_string(&path)
                .unwrap()
                .replacen("\"seq\":0", "\"seq\":0,\"note\":\"x\"", 1);
        fs::write(&path, text).unwrap();
        assert!(verify(&path).is_err());
    }

    #[test]
    fn missing_file_is_an_empty_chain() {
        let dir = tempfile::tempdir().unwrap();
        let summary = verify(&dir.path().join("none.jsonl")).unwrap();
        assert_eq!(summary.entries, 0);
        assert_eq!(summary.head, GENESIS);
    }
}
