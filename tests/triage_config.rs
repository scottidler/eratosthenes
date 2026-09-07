use std::process::Command;

// Phase 1 success criteria (design doc, docs/design/2026-07-06-llm-triage.md):
// `eratosthenes config validate` passes with and without a `triage:` block;
// a bucket missing `label` exits nonzero naming the offending bucket and field.

fn run_config_validate(config_path: &std::path::Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");
    Command::new(bin)
        .args(["--config"])
        .arg(config_path)
        .args(["config", "validate"])
        .output()
        .expect("failed to run eratosthenes binary")
}

#[test]
fn config_validate_passes_without_triage_block() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("account.yml");
    std::fs::write(
        &config_path,
        r#"
auth:
  creds-path: /tmp/creds
"#,
    )
    .unwrap();

    let output = run_config_validate(&config_path);
    assert!(
        output.status.success(),
        "config validate without a triage: block must pass, got:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_validate_passes_with_triage_block() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("account.yml");
    std::fs::write(
        &config_path,
        r#"
auth:
  creds-path: /tmp/creds
triage:
  schedule: "Mon..Fri 06:30:00"
"#,
    )
    .unwrap();

    let output = run_config_validate(&config_path);
    assert!(
        output.status.success(),
        "config validate with a minimal triage: block must pass, got:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Triage: configured"),
        "config validate should report triage as configured, got:\n{}",
        stdout
    );
}

#[test]
fn config_validate_fails_when_triage_block_missing_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("account.yml");
    std::fs::write(
        &config_path,
        r#"
auth:
  creds-path: /tmp/creds
triage:
  max-threads: 10
"#,
    )
    .unwrap();

    let output = run_config_validate(&config_path);
    assert!(
        !output.status.success(),
        "a triage: block with no schedule must be a load error"
    );
}

#[test]
fn config_validate_fails_naming_bucket_and_field_when_label_missing() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("account.yml");
    std::fs::write(
        &config_path,
        r#"
auth:
  creds-path: /tmp/creds
triage:
  schedule: "Mon..Fri 06:30:00"
  buckets:
    - name: noise
      description: everything else
"#,
    )
    .unwrap();

    let output = run_config_validate(&config_path);
    assert!(
        !output.status.success(),
        "a bucket missing label must exit nonzero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("noise"),
        "error must name the offending bucket, got:\n{}",
        stderr
    );
    assert!(
        stderr.contains("label"),
        "error must name the offending field, got:\n{}",
        stderr
    );
}
