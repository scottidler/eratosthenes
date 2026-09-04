use std::process::Command;

/// The binary dispatches the `digest` subcommand and its help renders.
#[test]
fn digest_help_dispatches() {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");
    let output = Command::new(bin)
        .args(["digest", "--help"])
        .output()
        .expect("failed to run eratosthenes binary");

    assert!(output.status.success(), "digest --help should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("pinned-inbox"),
        "digest --help should describe the pinned-inbox digest, got:\n{}",
        stdout
    );
}

/// Top-level help lists the `digest` subcommand.
#[test]
fn top_level_help_lists_digest() {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");
    let output = Command::new(bin)
        .arg("--help")
        .output()
        .expect("failed to run eratosthenes binary");

    assert!(output.status.success(), "--help should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("digest"),
        "top-level help should list the digest subcommand"
    );
}

/// `--dry-run` is `run`-only. It used to sit on the top-level command, where every
/// subcommand ACCEPTED it and only `run` honored it: `--dry-run digest` posted a live
/// digest to Slack.
#[test]
fn dry_run_is_run_only() {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");

    let accepted = Command::new(bin)
        .args(["run", "--dry-run", "--help"])
        .output()
        .expect("failed to run eratosthenes binary");
    assert!(
        accepted.status.success(),
        "run --dry-run must parse, got:\n{}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    for args in [
        vec!["--dry-run", "digest"],
        vec!["digest", "--dry-run"],
        vec!["--dry-run", "service", "install"],
    ] {
        let output = Command::new(bin)
            .args(&args)
            .output()
            .expect("failed to run eratosthenes binary");
        assert!(
            !output.status.success(),
            "{:?} must be REJECTED, not silently ignored",
            args
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--dry-run"),
            "{:?} should fail naming --dry-run, got:\n{}",
            args,
            stderr
        );
    }
}

/// The flag's help text must not claim more than it does: `ensure_labels` is not behind
/// the dry-run guard, so a dry run can still create missing labels.
#[test]
fn dry_run_help_admits_label_creation() {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");
    let output = Command::new(bin)
        .args(["run", "--help"])
        .output()
        .expect("failed to run eratosthenes binary");

    assert!(output.status.success(), "run --help should exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("missing labels may be created"),
        "run --help must say a dry run can still create labels, got:\n{}",
        stdout
    );
}

/// `--mark-only` is a MODE of `run`, not a new subcommand: it must be accepted there
/// and nowhere else.
#[test]
fn mark_only_is_a_run_flag_not_a_subcommand() {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");

    let accepted = Command::new(bin)
        .args(["run", "--mark-only", "--help"])
        .output()
        .expect("failed to run eratosthenes binary");
    assert!(
        accepted.status.success(),
        "run --mark-only must parse, got:\n{}",
        String::from_utf8_lossy(&accepted.stderr)
    );

    let rejected = Command::new(bin)
        .args(["digest", "--mark-only"])
        .output()
        .expect("failed to run eratosthenes binary");
    assert!(
        !rejected.status.success(),
        "digest --mark-only must be REJECTED, mark-only is run-only"
    );
}
