//! File-backed approval queue for approval-mode subscriptions.
//!
//! A queued item stores the verified envelope and the subscription id, not
//! the destination: the destination is resolved from the live config when
//! the item is approved, so a destination changed in the meantime is
//! honored.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::delivery::DeliveryEnvelope;
use crate::fsutil::write_json_atomic;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingApproval {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub subscription: String,
    pub envelope: DeliveryEnvelope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Approved,
    Rejected,
}

impl Resolution {
    fn dir(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
        }
    }
}

/// Approval ids are generated here; anything else is refused before it is
/// used as a file name.
fn is_approval_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

#[derive(Debug, Clone)]
pub struct ApprovalQueue {
    root: PathBuf,
}

impl ApprovalQueue {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, state: &str, id: &str) -> PathBuf {
        self.root.join(state).join(format!("{id}.json"))
    }

    /// Queue a verified envelope and return its approval id.
    pub fn enqueue(&self, envelope: &DeliveryEnvelope) -> Result<String> {
        let sha = envelope.brief_sha256()?;
        let id = format!(
            "{}-{}-{}",
            envelope.subscription,
            envelope.generated_at.format("%Y%m%d%H%M%S"),
            &sha[..8]
        );
        let item = PendingApproval {
            id: id.clone(),
            created_at: Utc::now(),
            subscription: envelope.subscription.clone(),
            envelope: envelope.clone(),
        };
        write_json_atomic(&self.path("pending", &id), &item)?;
        Ok(id)
    }

    /// Pending items, oldest first.
    pub fn list(&self) -> Result<Vec<PendingApproval>> {
        let dir = self.root.join("pending");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("reading {}", dir.display())),
        };
        let mut items = Vec::new();
        for entry in entries {
            let path = entry
                .with_context(|| format!("listing {}", dir.display()))?
                .path();
            if path.extension().is_some_and(|e| e == "json") {
                let bytes =
                    fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                items.push(
                    serde_json::from_slice::<PendingApproval>(&bytes)
                        .with_context(|| format!("parsing {}", path.display()))?,
                );
            }
        }
        items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(items)
    }

    pub fn get(&self, id: &str) -> Result<PendingApproval> {
        if !is_approval_id(id) {
            bail!("{id:?} is not an approval id");
        }
        let path = self.path("pending", id);
        let bytes = fs::read(&path).with_context(|| format!("no pending approval {id}"))?;
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
    }

    /// Move a pending item to `approved/` or `rejected/`.
    pub fn resolve(&self, id: &str, resolution: Resolution) -> Result<()> {
        let item = self.get(id)?;
        let from = self.path("pending", &item.id);
        let to = self.path(resolution.dir(), &item.id);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::tests::envelope;

    #[test]
    fn enqueue_list_and_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let queue = ApprovalQueue::new(dir.path().to_path_buf());
        let id = queue.enqueue(&envelope()).unwrap();
        assert!(id.starts_with("acme-rfp-"));
        let pending = queue.list().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, id);
        assert_eq!(pending[0].subscription, "acme-rfp");
        queue.resolve(&id, Resolution::Approved).unwrap();
        assert!(queue.list().unwrap().is_empty());
        assert!(
            dir.path()
                .join("approved")
                .join(format!("{id}.json"))
                .exists()
        );
        assert!(
            queue.get(&id).is_err(),
            "resolved items are no longer pending"
        );
    }

    #[test]
    fn traversal_ids_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let queue = ApprovalQueue::new(dir.path().to_path_buf());
        assert!(queue.get("../../etc/passwd").is_err());
        assert!(queue.resolve("A/B", Resolution::Rejected).is_err());
    }
}
