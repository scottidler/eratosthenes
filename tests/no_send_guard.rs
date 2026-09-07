//! The no-send guard.
//!
//! This design creates Gmail DRAFTS and never sends mail. That promise is one
//! adjacent call away from being broken: `google-gmail1 7.0.0+20251215` exposes
//! `messages_send(` (`api.rs:2130`) and `drafts_send(` (`api.rs:1733`) on the
//! same builders `GmailClient` already uses, and the second one sends the very
//! drafts `triage::draft` creates. Design doc Phase 0d measured both shapes and
//! pinned the guard pattern `\b(messages_send|drafts_send)\s*\(`.
//!
//! That pattern is implemented HERE in plain Rust rather than shelled out to
//! ripgrep, for two measured reasons: this host's ripgrep has no PCRE2 (a
//! `--pcre2` guard errors silently, and an `||` fallback around it reports a
//! false "clean" -- that exact mistake happened during Phase 0), and a guard
//! that depends on a tool being installed fails OPEN when it is not. The
//! semantics are identical: word boundary, either builder name, optional
//! whitespace, open paren.
//!
//! A guard that has never failed is not a guard, so `guard_bites_*` proves the
//! matcher fires on a real send call before `src_has_no_send_call` is allowed
//! to mean anything.

use std::fs;
use std::path::{Path, PathBuf};

/// The two send builders in `google-gmail1`, verbatim. There is no third.
const SEND_BUILDERS: [&str; 2] = ["messages_send", "drafts_send"];

/// One `\b(messages_send|drafts_send)\s*\(` match: 1-based line and the
/// builder that matched.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SendCall {
    line: usize,
    builder: String,
}

fn find_send_calls(source: &str) -> Vec<SendCall> {
    let mut hits: Vec<SendCall> = Vec::new();
    for builder in SEND_BUILDERS {
        for (idx, _) in source.match_indices(builder) {
            // `\b`: whatever precedes must not be a word character, so
            // `my_messages_send(` is not a hit on `messages_send`.
            let preceded_by_word = source[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
            if preceded_by_word {
                continue;
            }
            // `\s*\(`: a CALL, not a mention. This is what makes the guard
            // usable at all -- a bare `send` grep returns 257 lines against the
            // crate, nearly all of them `settings_send_as_*` alias settings.
            if !source[idx + builder.len()..].trim_start().starts_with('(') {
                continue;
            }
            hits.push(SendCall {
                line: source[..idx].matches('\n').count() + 1,
                builder: builder.to_string(),
            });
        }
    }
    hits.sort();
    hits
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("reading {}: {}", dir.display(), e));
    for entry in entries {
        let path = entry.expect("a readable dir entry").path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

fn scan(dir: &Path) -> Vec<String> {
    let mut findings = Vec::new();
    for path in rust_sources(dir) {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {}", path.display(), e));
        for hit in find_send_calls(&source) {
            findings.push(format!("{}:{}: {}(", path.display(), hit.line, hit.builder));
        }
    }
    findings
}

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// THE guard: no send call exists anywhere in the shipped binary.
#[test]
fn src_has_no_send_call() {
    let src = src_dir();
    let findings = scan(&src);
    assert!(
        findings.is_empty(),
        "this binary must never send mail, only draft it. Found {} send call(s):\n{}",
        findings.len(),
        findings.join("\n")
    );
}

/// A scan that walks nothing reports nothing, and would let the guard above
/// pass vacuously forever. Pin the floor.
#[test]
fn guard_actually_walks_the_source_tree() {
    let files = rust_sources(&src_dir());
    assert!(
        files.len() >= 15,
        "the guard scanned only {} file(s) under src/; it is looking in the wrong place",
        files.len()
    );
    assert!(
        files.iter().any(|p| p.ends_with("gmail/client.rs")),
        "the guard did not reach the module that holds every Gmail call"
    );
}

/// The bite, on the real call shapes Phase 0d measured against the crate.
#[test]
fn guard_matches_the_real_builder_shapes() {
    let messages_send_call = r#"
        self.hub
            .users()
            .messages_send(req, "me")
            .add_scope(GMAIL_SCOPE)
            .doit()
            .await
    "#;
    assert_eq!(
        find_send_calls(messages_send_call).len(),
        1,
        "the guard missed a messages.send call"
    );

    let drafts_send_call = r#"self.hub.users().drafts_send(req, "me").doit().await"#;
    assert_eq!(
        find_send_calls(drafts_send_call).len(),
        1,
        "the guard missed a drafts.send call -- the one that would send our own drafts"
    );

    // `\s*` spans a newline, so rustfmt cannot hide a send by breaking the line.
    let wrapped = "hub.users().drafts_send\n            (req, \"me\")";
    assert_eq!(
        find_send_calls(wrapped).len(),
        1,
        "the guard missed a send call split across lines"
    );
}

/// The other half of biting: not firing on things that send nothing. A guard
/// that matches everything gets disabled by the first person it annoys.
#[test]
fn guard_ignores_calls_that_send_nothing() {
    let benign = r#"
        hub.users().settings_send_as_list("me").doit().await;
        hub.users().settings_send_as_get("me", "alias").doit().await;
        let sent = channel.send(value);
        let helper = my_messages_send(req);
        // drafts_send is named in this comment but not called
        let name = "messages_send";
    "#;
    assert_eq!(
        find_send_calls(benign),
        Vec::new(),
        "the guard fired on code that sends no mail"
    );
}

/// End to end: the guard fails on a source tree that contains a send call.
/// This is the automated stand-in for the manual demonstration recorded in the
/// Phase 7 implementation notes, where a real `drafts_send` call was compiled
/// into `src/gmail/client.rs` and this test was watched to fail.
#[test]
fn guard_bites_on_an_injected_send() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let nested = dir.path().join("gmail");
    fs::create_dir_all(&nested).expect("a nested dir");
    fs::write(
        nested.join("client.rs"),
        "async fn oops(hub: &Hub) {\n    hub.users().drafts_send(req, \"me\").doit().await;\n}\n",
    )
    .expect("writing the injected send");

    let findings = scan(dir.path());
    assert_eq!(
        findings.len(),
        1,
        "the guard did not bite on an injected send call: {:?}",
        findings
    );
    assert!(
        findings[0].ends_with("client.rs:2: drafts_send("),
        "the guard must name file and line, got: {}",
        findings[0]
    );
}
