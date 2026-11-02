//! On-disk cache of per-thread aging facts, keyed by thread id and stamped with
//! the thread's `historyId`. Facts only, never decisions. No Gmail or engine
//! knowledge: callers decide when an entry is fresh.

use crate::gmail::message::GmailThread;
use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

pub const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCache {
    version: u32,
    threads: HashMap<String, CachedThread>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CachedThread {
    pub history_id: u64,
    /// Raw Gmail label ids, resolved to names at evaluation time.
    pub label_ids: Vec<String>,
    /// Newest message `internal_date`.
    pub last_activity: DateTime<Utc>,
    /// Newest message lacks `UNREAD`.
    pub is_read: bool,
}

impl CachedThread {
    /// `None` when the thread cannot yield trustworthy facts: no messages, no
    /// `history_id` to stamp it with, or a partial view (`skipped_messages`).
    pub fn from_thread(thread: &GmailThread) -> Option<CachedThread> {
        if thread.skipped_messages > 0 {
            return None;
        }
        let history_id = thread.history_id?;
        let last_activity = thread.last_activity()?;
        let mut label_ids: Vec<String> = thread.label_ids().into_iter().collect();
        label_ids.sort();
        Some(CachedThread {
            history_id,
            label_ids,
            last_activity,
            is_read: thread.is_read(),
        })
    }
}

impl Default for ThreadCache {
    fn default() -> Self {
        Self::empty()
    }
}

impl ThreadCache {
    pub fn empty() -> Self {
        ThreadCache {
            version: CACHE_VERSION,
            threads: HashMap::new(),
        }
    }

    /// Never errors: a lost cache costs fetches, never correctness. A missing
    /// file is the normal first run (DEBUG); unreadable, unparseable, or
    /// wrong-version files are WARN. All return an empty cache.
    pub fn load(path: &Path) -> ThreadCache {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                debug!("cache: {} not found, starting empty", path.display());
                return Self::empty();
            }
            Err(e) => {
                warn!("cache: cannot read {}: {e}; starting empty", path.display());
                return Self::empty();
            }
        };
        match serde_json::from_slice::<ThreadCache>(&bytes) {
            Ok(cache) if cache.version == CACHE_VERSION => {
                debug!(
                    "cache: loaded {} entries from {}",
                    cache.threads.len(),
                    path.display()
                );
                cache
            }
            Ok(cache) => {
                warn!(
                    "cache: {} has version {} (want {CACHE_VERSION}); starting empty",
                    path.display(),
                    cache.version
                );
                Self::empty()
            }
            Err(e) => {
                warn!(
                    "cache: cannot parse {}: {e}; starting empty",
                    path.display()
                );
                Self::empty()
            }
        }
    }

    /// Atomic: write `<path>.<pid>.tmp`, `sync_all`, rename into place. The
    /// pid suffix keeps two concurrent runs from interleaving into one temp file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create cache dir {}", parent.display()))?;
        }
        let bytes = serde_json::to_vec(self).context("serialize cache")?;
        let tmp = tmp_path(path);
        let written = (|| -> Result<()> {
            let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
            f.write_all(&bytes)
                .with_context(|| format!("write {}", tmp.display()))?;
            f.sync_all()
                .with_context(|| format!("sync {}", tmp.display()))?;
            fs::rename(&tmp, path)
                .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        written
    }

    pub fn get(&self, id: &str) -> Option<&CachedThread> {
        self.threads.get(id)
    }

    pub fn insert(&mut self, id: String, thread: CachedThread) {
        self.threads.insert(id, thread);
    }

    pub fn remove(&mut self, id: &str) -> Option<CachedThread> {
        self.threads.remove(id)
    }

    /// Keep only entries whose id is in `ids`; returns how many were dropped.
    pub fn retain_ids(&mut self, ids: &HashSet<&str>) -> usize {
        let before = self.threads.len();
        self.threads.retain(|id, _| ids.contains(id.as_str()));
        before - self.threads.len()
    }

    pub fn len(&self) -> usize {
        self.threads.len()
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }
}

fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{}.tmp", std::process::id()));
    PathBuf::from(name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gmail::message::GmailMessage;
    use chrono::TimeZone;

    fn msg(labels: &[&str], date: DateTime<Utc>) -> GmailMessage {
        GmailMessage {
            id: "m".to_string(),
            thread_id: "t".to_string(),
            label_ids: labels.iter().map(|s| s.to_string()).collect(),
            internal_date: date,
            headers: HashMap::new(),
            to: vec![],
            cc: vec![],
            from: vec![],
            subject: String::new(),
        }
    }

    fn thread(history_id: Option<u64>, skipped: usize, messages: Vec<GmailMessage>) -> GmailThread {
        GmailThread {
            id: "t".to_string(),
            history_id,
            messages,
            skipped_messages: skipped,
        }
    }

    fn entry(h: u64) -> CachedThread {
        CachedThread {
            history_id: h,
            label_ids: vec!["INBOX".into(), "UNREAD".into()],
            last_activity: Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap(),
            is_read: false,
        }
    }

    #[test]
    fn round_trip_equal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/acct.json");
        let mut c = ThreadCache::empty();
        c.insert("a".into(), entry(1));
        c.insert("b".into(), entry(2));
        c.save(&path).unwrap();
        assert_eq!(ThreadCache::load(&path), c);
    }

    #[test]
    fn missing_file_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(ThreadCache::load(&dir.path().join("nope.json")).is_empty());
    }

    #[test]
    fn garbage_bytes_load_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        fs::write(&path, [0xff, 0x00, 0x7b, 0x9c]).unwrap();
        assert!(ThreadCache::load(&path).is_empty());
    }

    #[test]
    fn version_mismatch_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let mut c = ThreadCache::empty();
        c.insert("a".into(), entry(1));
        c.version = 0;
        fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
        assert!(ThreadCache::load(&path).is_empty());
    }

    #[test]
    fn save_leaves_no_tmp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        ThreadCache::empty().save(&path).unwrap();
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["c.json".to_string()]);
    }

    #[test]
    fn save_failure_is_an_error_and_cleans_tmp() {
        let dir = tempfile::tempdir().unwrap();
        // destination is a non-empty directory: rename must fail
        let path = dir.path().join("c.json");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("x"), b"x").unwrap();
        assert!(ThreadCache::empty().save(&path).is_err());
        assert!(!tmp_path(&path).exists());
    }

    #[test]
    fn serialized_keys_are_kebab_case() {
        let json = serde_json::to_string(&entry(7)).unwrap();
        assert!(json.contains("\"history-id\":7"));
        assert!(json.contains("\"last-activity\""));
        assert!(json.contains("\"is-read\""));
    }

    #[test]
    fn get_insert_remove_retain() {
        let mut c = ThreadCache::empty();
        assert!(c.get("a").is_none());
        c.insert("a".into(), entry(1));
        c.insert("b".into(), entry(2));
        c.insert("c".into(), entry(3));
        assert_eq!(c.get("a"), Some(&entry(1)));
        assert_eq!(c.remove("a"), Some(entry(1)));
        assert!(c.remove("a").is_none());
        let keep: HashSet<&str> = ["b", "zzz"].into_iter().collect();
        assert_eq!(c.retain_ids(&keep), 1);
        assert!(c.get("b").is_some());
        assert!(c.get("c").is_none());
        assert_eq!(c.retain_ids(&keep), 0);
    }

    #[test]
    fn from_thread_builds_facts_from_existing_definitions() {
        let early = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 10, 2, 0, 0, 0).unwrap();
        let t = thread(
            Some(42),
            0,
            vec![
                msg(&["INBOX", "UNREAD"], early),
                msg(&["INBOX", "STARRED"], late),
            ],
        );
        let c = CachedThread::from_thread(&t).unwrap();
        assert_eq!(c.history_id, 42);
        assert_eq!(c.last_activity, late);
        assert!(c.is_read);
        assert_eq!(c.label_ids, vec!["INBOX", "STARRED", "UNREAD"]);
    }

    #[test]
    fn from_thread_none_without_messages() {
        assert!(CachedThread::from_thread(&thread(Some(1), 0, vec![])).is_none());
    }

    #[test]
    fn from_thread_none_without_history_id() {
        let t = thread(None, 0, vec![msg(&[], Utc::now())]);
        assert!(CachedThread::from_thread(&t).is_none());
    }

    #[test]
    fn from_thread_none_when_messages_skipped() {
        let t = thread(Some(1), 1, vec![msg(&[], Utc::now())]);
        assert!(CachedThread::from_thread(&t).is_none());
    }
}
