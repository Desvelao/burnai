//! Approval of proposed batches. An approval is single-use: it never authorizes future batches.
use crate::models::{Confidence, TaskStatus};
use crate::scheduler::Proposal;
use crate::{tasks, Error, Result};
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingBatch {
    pub id: u64,
    pub proposal: Proposal,
    pub confidence: Confidence,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedBatch {
    pub account_id: i64,
    pub task_ids: Vec<i64>,
    /// Carried over from the proposal ("Burn now").
    pub burn: bool,
}

#[derive(Debug, Default)]
pub struct ApprovalManager {
    next: u64,
    pending: HashMap<u64, PendingBatch>,
}

impl ApprovalManager {
    /// Registers a batch for user approval and marks its tasks AwaitingApproval.
    pub fn request(
        &mut self,
        conn: &Connection,
        proposal: Proposal,
        confidence: Confidence,
        now: DateTime<Utc>,
    ) -> Result<u64> {
        // Don't propose the same task twice.
        for b in self.pending.values() {
            if b.proposal
                .tasks
                .iter()
                .any(|t| proposal.tasks.iter().any(|n| n.id == t.id))
            {
                return Err(Error::Blocked(
                    "a task in this batch is already awaiting approval".into(),
                ));
            }
        }
        self.next += 1;
        for t in &proposal.tasks {
            tasks::set_status(conn, t.id, TaskStatus::AwaitingApproval)?;
        }
        let id = self.next;
        self.pending.insert(
            id,
            PendingBatch {
                id,
                proposal,
                confidence,
                created_at: now,
            },
        );
        Ok(id)
    }

    pub fn pending(&self) -> Vec<&PendingBatch> {
        let mut v: Vec<_> = self.pending.values().collect();
        v.sort_by_key(|b| b.id);
        v
    }

    pub fn has_pending_for_account(&self, account_id: i64) -> bool {
        self.pending
            .values()
            .any(|b| b.proposal.account_id == account_id)
    }

    /// Approves the selected tasks (a subset of the batch). Unselected tasks go back to Pending.
    /// The batch is consumed: a second call fails.
    pub fn approve(
        &mut self,
        conn: &Connection,
        batch_id: u64,
        selected: &[i64],
    ) -> Result<ApprovedBatch> {
        let b = self
            .pending
            .remove(&batch_id)
            .ok_or_else(|| Error::NotFound("approval request (already used or expired)".into()))?;
        let mut ids = Vec::new();
        for t in &b.proposal.tasks {
            if selected.contains(&t.id) {
                ids.push(t.id);
            } else {
                tasks::set_status(conn, t.id, TaskStatus::Pending)?;
            }
        }
        if ids.is_empty() {
            return Err(Error::Invalid("no tasks selected".into()));
        }
        Ok(ApprovedBatch {
            account_id: b.proposal.account_id,
            task_ids: ids,
            burn: b.proposal.burn,
        })
    }

    pub fn reject(&mut self, conn: &Connection, batch_id: u64) -> Result<()> {
        if let Some(b) = self.pending.remove(&batch_id) {
            for t in &b.proposal.tasks {
                tasks::set_status(conn, t.id, TaskStatus::Pending)?;
            }
        }
        Ok(())
    }

    /// Used by emergency STOP: drop every outstanding request.
    pub fn cancel_all(&mut self, conn: &Connection) -> Result<usize> {
        let ids: Vec<u64> = self.pending.keys().copied().collect();
        for id in &ids {
            self.reject(conn, *id)?;
        }
        Ok(ids.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::tasks::NewTask;

    fn setup() -> (Connection, Proposal) {
        let c = db::open_memory().unwrap();
        let a = tasks::create(
            &c,
            &NewTask {
                name: "a".into(),
                prompt: "p".into(),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap();
        let b = tasks::create(
            &c,
            &NewTask {
                name: "b".into(),
                prompt: "p".into(),
                ..Default::default()
            },
            TaskStatus::Pending,
        )
        .unwrap();
        (
            c,
            Proposal {
                account_id: 1,
                tasks: vec![a, b],
                est_consumption: 10.0,
                est_minutes: 20,
                auto: false,
                burn: false,
            },
        )
    }

    #[test]
    fn approval_is_single_use_and_supports_subset() {
        let (c, p) = setup();
        let ids: Vec<i64> = p.tasks.iter().map(|t| t.id).collect();
        let mut m = ApprovalManager::default();
        let bid = m.request(&c, p, Confidence::Manual, Utc::now()).unwrap();
        assert_eq!(
            tasks::get(&c, ids[0]).unwrap().status,
            TaskStatus::AwaitingApproval
        );
        let ok = m.approve(&c, bid, &[ids[0]]).unwrap();
        assert_eq!(ok.task_ids, vec![ids[0]]);
        assert_eq!(tasks::get(&c, ids[1]).unwrap().status, TaskStatus::Pending);
        assert!(
            m.approve(&c, bid, &[ids[0]]).is_err(),
            "approval cannot be reused for a future batch"
        );
    }

    #[test]
    fn rejection_returns_tasks_to_pending() {
        let (c, p) = setup();
        let ids: Vec<i64> = p.tasks.iter().map(|t| t.id).collect();
        let mut m = ApprovalManager::default();
        let bid = m.request(&c, p, Confidence::Manual, Utc::now()).unwrap();
        m.reject(&c, bid).unwrap();
        assert!(ids
            .iter()
            .all(|i| tasks::get(&c, *i).unwrap().status == TaskStatus::Pending));
        assert!(m.pending().is_empty());
    }

    #[test]
    fn stop_cancels_all_requests() {
        let (c, p) = setup();
        let mut m = ApprovalManager::default();
        m.request(&c, p, Confidence::Manual, Utc::now()).unwrap();
        assert_eq!(m.cancel_all(&c).unwrap(), 1);
        assert!(m.pending().is_empty());
    }
}
