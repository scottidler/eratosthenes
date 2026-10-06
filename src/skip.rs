//! Per-account-run accounting for work skipped on a thread-scoped Gmail error.
//!
//! One ledger per account run, shared across every engine phase, so a thread
//! skipped early gets no further writes later and the ceiling counts distinct
//! ids across the whole run rather than failed calls per phase.

use std::collections::HashSet;

use eyre::Result;
use log::{debug, warn};

use crate::gmail::rate::{ErrorScope, error_scope};

pub(crate) struct SkipLedger {
    ids: HashSet<String>,
    max: usize,
}

impl SkipLedger {
    pub(crate) fn new(max: usize) -> Self {
        Self {
            ids: HashSet::new(),
            max,
        }
    }

    /// Distinct ids skipped so far this run.
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// The per-thread boundary: a `Thread`-scope failure is logged, recorded,
    /// and swallowed (`Ok`), unless it pushes the run over the ceiling; any
    /// `Account`-scope failure comes back unchanged so it fails the run.
    pub(crate) fn skip_thread(
        &mut self,
        thread_id: &str,
        op: &str,
        err: eyre::Report,
        prefix: &str,
    ) -> Result<()> {
        self.absorb("thread", thread_id, op, err, prefix)
    }

    fn absorb(
        &mut self,
        noun: &str,
        id: &str,
        op: &str,
        err: eyre::Report,
        prefix: &str,
    ) -> Result<()> {
        let scope = error_scope(&err);
        debug!(
            "{}skip ledger: {} {} op={} scope={:?} skipped_so_far={} max={}",
            prefix,
            noun,
            id,
            op,
            scope,
            self.ids.len(),
            self.max
        );
        if scope == ErrorScope::Account {
            return Err(err);
        }

        warn!("{}{}", prefix, skip_warning(noun, id, op, &err));
        self.ids.insert(id.to_string());

        // Checked at each insert, not at end of run: a systemic failure stops
        // after max+1 calls instead of burning the whole run's worth. The text
        // deliberately omits the per-thread causes (those are the WARNs above),
        // so a ceiling failure never reads as a bare FAILED_PRECONDITION.
        if self.ids.len() > self.max {
            eyre::bail!(
                "skipped {} distinct threads/messages, over max-skipped-threads {}",
                self.ids.len(),
                self.max
            );
        }
        Ok(())
    }
}

/// The skip WARN body. Operators grep for it (`skipping thread <id>: <op>
/// failed: <cause>`), so the shape is pinned by a test.
fn skip_warning(noun: &str, id: &str, op: &str, err: &eyre::Report) -> String {
    format!("skipping {} {}: {} failed: {:#}", noun, id, op, err)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn failed_precondition() -> eyre::Report {
        eyre::Report::new(google_gmail1::Error::BadRequest(serde_json::json!({
            "error": { "code": 400, "status": "FAILED_PRECONDITION" }
        })))
    }

    #[test]
    fn test_thread_scope_is_recorded_and_swallowed() {
        let mut ledger = SkipLedger::new(10);
        ledger
            .skip_thread("t1", "threads.get", failed_precondition(), "")
            .unwrap();
        assert_eq!(ledger.len(), 1);
        assert!(ledger.contains("t1"));
        assert!(!ledger.contains("t2"));
    }

    #[test]
    fn test_account_scope_propagates_unchanged_and_is_not_recorded() {
        let mut ledger = SkipLedger::new(10);
        let err = ledger
            .skip_thread("t1", "threads.get", eyre::eyre!("auth expired"), "")
            .unwrap_err();
        assert_eq!(err.to_string(), "auth expired");
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn test_ceiling_counts_distinct_ids() {
        let mut ledger = SkipLedger::new(1);
        ledger
            .skip_thread("t1", "threads.get", failed_precondition(), "")
            .unwrap();
        // Same id again: still one distinct skip, still under the ceiling.
        ledger
            .skip_thread("t1", "threads.modify", failed_precondition(), "")
            .unwrap();
        assert_eq!(ledger.len(), 1);
        let err = ledger
            .skip_thread("t2", "threads.get", failed_precondition(), "")
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "skipped 2 distinct threads/messages, over max-skipped-threads 1"
        );
        assert!(!format!("{err:#}").contains("FAILED_PRECONDITION"));
    }

    #[test]
    fn test_zero_ceiling_fails_on_the_first_skip() {
        let mut ledger = SkipLedger::new(0);
        assert!(
            ledger
                .skip_thread("t1", "threads.get", failed_precondition(), "")
                .is_err()
        );
    }

    /// The operator acceptance check is
    /// `rg -z 'skipping (thread|message) .*FAILED_PRECONDITION'`.
    #[test]
    fn test_skip_warning_carries_id_op_and_cause() {
        use eyre::Context;
        let err = Err::<(), _>(google_gmail1::Error::BadRequest(serde_json::json!({
            "error": { "code": 400, "status": "FAILED_PRECONDITION" }
        })))
        .context("threads.get(19fa97828f656eb1) failed")
        .unwrap_err();
        let line = skip_warning("thread", "19fa97828f656eb1", "threads.get", &err);
        assert!(
            line.starts_with("skipping thread 19fa97828f656eb1: threads.get failed: "),
            "{line}"
        );
        assert!(line.contains("FAILED_PRECONDITION"), "{line}");
    }
}
