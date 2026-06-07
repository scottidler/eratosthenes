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
