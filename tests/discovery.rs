use std::path::Path;
use std::process::Command;

// Implementation audit F1 (docs/plan/2026-11-12-inbox-gate.md): account discovery
// runs before logging is set up, so a config that fails to load must be a hard
// error naming the file, never a skipped account. These tests point discovery at a
// temp config dir through XDG_CONFIG_HOME, with no --config flag.

const HOME_YML: &str = r#"
auth:
  creds-path: /tmp/creds-home
  callback-port: 13131
"#;

const TATARI_YML: &str = r#"
auth:
  creds-path: /tmp/creds-tatari
  callback-port: 13132
triage:
  schedule: "Mon..Fri 06:30:00"
  buckets:
    - name: noise
      label: llm/noise
      description: everything else
"#;

fn write_account(xdg: &Path, name: &str, content: &str) {
    let dir = xdg.join("eratosthenes");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{}.yml", name)), content).unwrap();
}

fn run_discovered_config_validate(xdg: &Path) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_eratosthenes");
    Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg)
        .args(["config", "validate"])
        .output()
        .expect("failed to run eratosthenes binary")
}

#[test]
fn discovered_account_with_unknown_triage_key_fails_validate() {
    let xdg = tempfile::tempdir().unwrap();
    write_account(xdg.path(), "home", HOME_YML);
    write_account(
        xdg.path(),
        "tatari",
        &TATARI_YML.replace(
            "description: everything else",
            "description: everything else\n      archvie: true",
        ),
    );

    let output = run_discovered_config_validate(xdg.path());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a discovered config with an unknown key must fail validate, stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("tatari.yml"), "got:\n{}", stderr);
    assert!(stderr.contains("archvie"), "got:\n{}", stderr);
}

#[test]
fn valid_discovered_accounts_validate() {
    let xdg = tempfile::tempdir().unwrap();
    write_account(xdg.path(), "home", HOME_YML);
    write_account(xdg.path(), "tatari", TATARI_YML);

    let output = run_discovered_config_validate(xdg.path());
    assert!(
        output.status.success(),
        "two valid discovered accounts must validate, got:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Account 'home' config is valid."),
        "{}",
        stdout
    );
    assert!(
        stdout.contains("Account 'tatari' config is valid."),
        "{}",
        stdout
    );
}
