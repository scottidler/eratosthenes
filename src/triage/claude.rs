//! Keyless LLM transport: a subprocess to the locally installed `claude` CLI
//! in headless print mode.
//!
//! This module holds NO credential and forwards none. The child owns its own
//! auth (design doc, Resolved Decisions 2026-09-06). Two properties here are
//! load-bearing for the Security section and must not be "simplified":
//!
//! 1. The hardening argv (`build_args`). Without `--tools ""` and
//!    `--safe-mode`, an adversarial email body is processed by a full Claude
//!    Code session on this host, with tools live and this machine's CLAUDE.md,
//!    skills, hooks and MCP servers loaded. With them the child is a pure data
//!    transformer.
//! 2. The built environment (`child_env`). A live agent session on this host
//!    carries `ANTHROPIC*` secrets including an admin key; an inherit-by-default
//!    child would hand them to every triage run.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, info, trace};
use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;

/// Version floor, MEASURED in Phase 0c on desk.lan: the seven-flag argv below
/// is accepted by `claude` 2.1.263 and returns a parsed envelope at exit 0.
/// Logged on resolve and named in every failure, never pre-flight GATED: the
/// version string's format is foreign, and a brittle parse would fail closed on
/// a CLI that actually works.
pub const MIN_CLAUDE_VERSION: &str = "2.1.263";

/// Provisional per-call ceiling for the triage classify call (design doc, Open
/// Questions). Phase 4's live runs confirm or raise it; the recorded rule is
/// "at least 2x measured".
pub const TRIAGE_TIMEOUT: Duration = Duration::from_secs(300);

/// Bound on `claude --version`, which is a local no-network call. Separate from
/// `TRIAGE_TIMEOUT` so a hung version probe cannot burn the classify budget.
const VERSION_TIMEOUT: Duration = Duration::from_secs(30);

/// What KIND of failure this was. The digest banner names the class (design
/// doc, Resolved Decisions): an expired login exits cleanly non-zero forever,
/// and "bullets unavailable" alone makes a weeks-long outage look like a blip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The binary did not resolve. Under systemd this is a PATH problem, not
    /// an install problem (Phase 0c).
    NotFound,
    /// The CLI is installed but not logged in. Silent and permanent until a
    /// human re-authenticates.
    Auth,
    /// Throttled upstream. Transient; the next timer fire retries.
    RateLimited,
    /// Exceeded the call ceiling and was killed and reaped.
    Timeout,
    /// Network or upstream error.
    Transport,
    /// Exit 0 but the output was not the envelope we asked for. Version drift
    /// looks like this.
    Protocol,
}

impl FailureClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            FailureClass::NotFound => "claude not found",
            FailureClass::Auth => "claude not authenticated",
            FailureClass::RateLimited => "claude rate limited",
            FailureClass::Timeout => "claude timed out",
            FailureClass::Transport => "claude transport failure",
            FailureClass::Protocol => "claude protocol failure",
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// A classified transport failure that always names the version floor, so an
/// unsupported-flag exit reads as "your claude is older than the floor"
/// instead of as a mystery.
#[derive(Debug)]
pub struct ClaudeFailure {
    pub class: FailureClass,
    pub detail: String,
    pub version: Option<String>,
}

impl ClaudeFailure {
    fn new(class: FailureClass, detail: impl Into<String>, version: Option<String>) -> Self {
        Self {
            class,
            detail: detail.into(),
            version,
        }
    }
}

impl std::fmt::Display for ClaudeFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: {} (resolved claude version: {}, floor: {})",
            self.class,
            self.detail,
            self.version.as_deref().unwrap_or("unknown"),
            MIN_CLAUDE_VERSION
        )
    }
}

impl std::error::Error for ClaudeFailure {}

/// The production argv, in full. Every flag past `--model` is a hardening flag
/// and the whole basis of the blast-radius claim in the design doc's Security
/// section. Phase 0c proved this exact shape against `claude` 2.1.263.
///
/// `--tools ""` is TWO argv elements -- `--tools` then an empty string -- not
/// one joined token. Deliberately NOT passed: `--fallback-model` (the CLI must
/// not silently swap out a pinned model) and `--system-prompt` (there is no
/// second transport to keep in sync, and the instruction rides `-p`).
pub fn build_args(model: &str, prompt: &str) -> Vec<String> {
    vec![
        "-p".to_string(),
        prompt.to_string(),
        "--model".to_string(),
        model.to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        "--tools".to_string(),
        String::new(),
        "--safe-mode".to_string(),
        "--strict-mcp-config".to_string(),
        "--no-session-persistence".to_string(),
        "--max-turns".to_string(),
        "1".to_string(),
    ]
}

/// The child's environment, BUILT rather than inherited. Every variable not
/// listed here is absent by construction, which is what excludes `ANTHROPIC*`
/// (this design forwards no key, ever) and `CLAUDE_CODE_*` (a triage run must
/// not present itself to the child as a nested session).
///
/// `NO_UPDATE_NOTIFIER=1` is the belt against an npm-installed Claude Code
/// printing an update notice ahead of the JSON; `parse_envelope`'s tolerance
/// for leading noise is the suspenders.
pub fn child_env_from<F>(get: F) -> Vec<(String, String)>
where
    F: Fn(&str) -> Option<String>,
{
    const ALLOWED: &[&str] = &["HOME", "USER", "PATH"];
    let mut env: Vec<(String, String)> = ALLOWED
        .iter()
        .filter_map(|key| get(key).map(|value| (key.to_string(), value)))
        .collect();
    env.push(("NO_UPDATE_NOTIFIER".to_string(), "1".to_string()));
    env
}

pub fn child_env() -> Vec<(String, String)> {
    child_env_from(|key| std::env::var(key).ok())
}

/// The `claude -p --output-format json` envelope. Only `result` is consumed;
/// the rest of the envelope's fields are ignored deliberately, so a CLI that
/// ADDS fields does not become a parse failure.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

/// Pull the `result` string out of the envelope, tolerating leading noise on
/// stdout. Tolerant because an npm-installed Claude Code can print an update
/// notice ahead of the JSON; clyde hit exactly this.
pub fn parse_envelope(stdout: &str) -> Result<String, ClaudeFailure> {
    trace!("parse_envelope: chars={}", stdout.len());

    for (idx, _) in stdout.match_indices('{') {
        let mut stream = serde_json::Deserializer::from_str(&stdout[idx..]).into_iter::<Envelope>();
        let Some(Ok(envelope)) = stream.next() else {
            continue;
        };

        if envelope.is_error {
            let detail = envelope.result.unwrap_or_else(|| {
                envelope
                    .subtype
                    .unwrap_or_else(|| "envelope is_error=true".to_string())
            });
            return Err(ClaudeFailure::new(classify_text(&detail), detail, None));
        }
        return envelope.result.ok_or_else(|| {
            ClaudeFailure::new(
                FailureClass::Protocol,
                "envelope carried no `result` field",
                None,
            )
        });
    }

    Err(ClaudeFailure::new(
        FailureClass::Protocol,
        format!(
            "no JSON envelope on stdout ({} chars): {}",
            stdout.len(),
            stdout.chars().take(200).collect::<String>()
        ),
        None,
    ))
}

/// Map a stderr or error-envelope string onto a failure class. Substring
/// matching, deliberately: the CLI's error text is not a stable contract, so an
/// unrecognized message must degrade to `Transport` rather than to a panic or a
/// wrong class.
pub fn classify_text(text: &str) -> FailureClass {
    let lower = text.to_lowercase();
    const AUTH: &[&str] = &[
        "not authenticated",
        "authentication",
        "unauthorized",
        "401",
        "invalid api key",
        "please run /login",
        "log in",
        "login",
        "credit balance",
    ];
    const RATE: &[&str] = &["rate limit", "rate_limit", "429", "quota", "overloaded"];

    if AUTH.iter().any(|needle| lower.contains(needle)) {
        return FailureClass::Auth;
    }
    if RATE.iter().any(|needle| lower.contains(needle)) {
        return FailureClass::RateLimited;
    }
    FailureClass::Transport
}

/// A resolved `claude` binary plus the version it reported.
#[derive(Debug)]
pub struct ClaudeCli {
    binary: PathBuf,
    version: String,
    timeout: Duration,
}

impl ClaudeCli {
    /// Resolve the binary (config `claude-binary` if set, else PATH) and record
    /// its version. Resolution failure is reported HERE, once, rather than as a
    /// mysterious spawn error later.
    pub async fn resolve(
        configured: Option<&Path>,
        call_timeout: Duration,
    ) -> Result<Self, ClaudeFailure> {
        let binary = configured
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("claude"));
        debug!("ClaudeCli::resolve: binary={}", binary.display());

        let mut cmd = Command::new(&binary);
        cmd.arg("--version");
        cmd.env_clear();
        for (key, value) in child_env() {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::null());

        let output = match timeout(VERSION_TIMEOUT, cmd.output()).await {
            Err(_) => {
                return Err(ClaudeFailure::new(
                    FailureClass::Timeout,
                    format!("`{} --version` did not return", binary.display()),
                    None,
                ));
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ClaudeFailure::new(
                    FailureClass::NotFound,
                    format!(
                        "`{}` did not resolve; under systemd the unit's own PATH is what matters, not your shell's",
                        binary.display()
                    ),
                    None,
                ));
            }
            Ok(Err(e)) => {
                return Err(ClaudeFailure::new(
                    FailureClass::Transport,
                    format!("spawning `{} --version` failed: {}", binary.display(), e),
                    None,
                ));
            }
            Ok(Ok(output)) => output,
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(ClaudeFailure::new(
                classify_text(&stderr),
                format!(
                    "`{} --version` exited nonzero: {}",
                    binary.display(),
                    stderr
                ),
                None,
            ));
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!(
            "claude resolved: binary={}, version={}, floor={}",
            binary.display(),
            version,
            MIN_CLAUDE_VERSION
        );

        Ok(Self {
            binary,
            version,
            timeout: call_timeout,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// One headless call. The fixed instruction rides argv (`-p`); the thread
    /// bodies ride a temp FILE on stdin.
    ///
    /// File, not pipe, and the distinction is the whole point: with the child's
    /// stdin/stdout/stderr wired to files no pipe exists, so no pipe can fill
    /// and no drain can deadlock. Say "stdin" without saying "file" and a
    /// reader implements the deadlock this avoids.
    pub async fn invoke(
        &self,
        model: &str,
        prompt: &str,
        payload: &str,
    ) -> Result<String, ClaudeFailure> {
        debug!(
            "ClaudeCli::invoke: model={}, prompt_chars={}, payload_chars={}, timeout={}s",
            model,
            prompt.chars().count(),
            payload.chars().count(),
            self.timeout.as_secs()
        );

        let workdir = self.scratch_dir()?;
        let stdin_path = workdir.path().join("payload.txt");
        let stdout_path = workdir.path().join("stdout.json");
        let stderr_path = workdir.path().join("stderr.txt");

        self.io_err(fs::write(&stdin_path, payload), "writing payload file")?;
        let stdin = self.io_err(File::open(&stdin_path), "opening payload file")?;
        let stdout = self.io_err(File::create(&stdout_path), "creating stdout file")?;
        let stderr = self.io_err(File::create(&stderr_path), "creating stderr file")?;

        let mut cmd = Command::new(&self.binary);
        cmd.args(build_args(model, prompt));
        cmd.env_clear();
        for (key, value) in child_env() {
            cmd.env(key, value);
        }
        cmd.stdin(Stdio::from(stdin));
        cmd.stdout(Stdio::from(stdout));
        cmd.stderr(Stdio::from(stderr));
        cmd.kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(self.fail(
                    FailureClass::NotFound,
                    format!("`{}` did not resolve", self.binary.display()),
                ));
            }
            Err(e) => {
                return Err(self.fail(
                    FailureClass::Transport,
                    format!("spawning `{}` failed: {}", self.binary.display(), e),
                ));
            }
        };

        // Kill AND reap. Killing alone leaves a zombie behind on every timeout,
        // and the timer would accumulate them.
        let status = match timeout(self.timeout, child.wait()).await {
            Ok(Ok(status)) => status,
            Ok(Err(e)) => {
                return Err(self.fail(FailureClass::Transport, format!("wait failed: {}", e)));
            }
            Err(_) => {
                let killed = child.start_kill();
                let reaped = child.wait().await;
                return Err(self.fail(
                    FailureClass::Timeout,
                    format!(
                        "no response within {}s; kill={:?}, reap={:?}",
                        self.timeout.as_secs(),
                        killed.map(|_| "sent"),
                        reaped.map(|s| s.code())
                    ),
                ));
            }
        };

        let out = fs::read_to_string(&stdout_path).unwrap_or_default();
        let err = fs::read_to_string(&stderr_path).unwrap_or_default();

        if !status.success() {
            let detail = format!(
                "exited {}: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string()),
                err.trim()
            );
            return Err(self.fail(classify_text(&err), detail));
        }

        parse_envelope(&out).map_err(|e| self.fail(e.class, e.detail))
    }

    fn fail(&self, class: FailureClass, detail: impl Into<String>) -> ClaudeFailure {
        ClaudeFailure::new(class, detail, Some(self.version.clone()))
    }

    fn io_err<T>(&self, result: std::io::Result<T>, what: &str) -> Result<T, ClaudeFailure> {
        result.map_err(|e| self.fail(FailureClass::Transport, format!("{}: {}", what, e)))
    }

    fn scratch_dir(&self) -> Result<ScratchDir, ClaudeFailure> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "eratosthenes-triage-{}-{}",
            std::process::id(),
            nanos
        ));
        self.io_err(fs::create_dir_all(&path), "creating scratch dir")?;
        Ok(ScratchDir { path })
    }
}

/// Removes the child's stdin/stdout/stderr files when the call returns, on
/// every path including the error ones. Those files hold work-mail bodies;
/// leaving them in `/tmp` would turn a transient prompt payload into an
/// indefinite one.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.path) {
            log::warn!(
                "failed to remove scratch dir {}: {}",
                self.path.display(),
                e
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The seven-flag argv, element by element. This test is the guard on the
    /// design doc's Security claim: any change to it is a change to the
    /// blast-radius argument and must be made deliberately.
    #[test]
    fn test_build_args_is_the_hardened_seven_flag_shape() {
        let args = build_args("claude-haiku-4-5-20251001", "classify these");
        assert_eq!(
            args,
            vec![
                "-p",
                "classify these",
                "--model",
                "claude-haiku-4-5-20251001",
                "--output-format",
                "json",
                "--tools",
                "",
                "--safe-mode",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--max-turns",
                "1",
            ]
        );
    }

    /// `--tools ""` is TWO argv elements. One joined token (`--tools=` or
    /// `--tools ""` as a single string) is a different, unproven invocation.
    #[test]
    fn test_tools_flag_is_two_argv_elements() {
        let args = build_args("m", "p");
        let idx = args
            .iter()
            .position(|a| a == "--tools")
            .expect("--tools present");
        assert_eq!(args[idx + 1], "", "the empty string is its own element");
        assert!(
            !args
                .iter()
                .any(|a| a.starts_with("--tools=") || a == "--tools \"\""),
            "argv must not carry a joined --tools token: {:?}",
            args
        );
    }

    #[test]
    fn test_build_args_omits_deliberately_excluded_flags() {
        let args = build_args("m", "p");
        assert!(!args.iter().any(|a| a == "--fallback-model"));
        assert!(!args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn test_child_env_is_an_allowlist_and_carries_the_update_notifier() {
        let env = child_env_from(|key| match key {
            "HOME" => Some("/home/saidler".to_string()),
            "USER" => Some("saidler".to_string()),
            "PATH" => Some("/usr/bin".to_string()),
            _ => None,
        });
        assert_eq!(
            env,
            vec![
                ("HOME".to_string(), "/home/saidler".to_string()),
                ("USER".to_string(), "saidler".to_string()),
                ("PATH".to_string(), "/usr/bin".to_string()),
                ("NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
            ]
        );
    }

    /// The measured bug this guards: a live agent session carries `ANTHROPIC*`
    /// secrets, one of them an admin key. An inherit-by-default child hands
    /// them to every triage run.
    #[test]
    fn test_child_env_excludes_anthropic_and_claude_code_vars() {
        let env = child_env_from(|key| Some(format!("value-of-{}", key)));
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        for leaked in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_COST_ANTHROPIC_API_ADMIN_KEY",
            "CLAUDE_CODE_SESSION_ID",
            "SLACK_XOXP_TOKEN",
        ] {
            assert!(!keys.contains(&leaked), "{} leaked into child env", leaked);
        }
        assert!(!keys.iter().any(|k| k.starts_with("ANTHROPIC")));
        assert!(!keys.iter().any(|k| k.starts_with("CLAUDE_CODE_")));
    }

    #[test]
    fn test_parse_envelope_extracts_result() {
        let stdout = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;
        assert_eq!(parse_envelope(stdout).unwrap(), "ok");
    }

    /// An npm-installed Claude Code can print an update notice ahead of the
    /// JSON; the parse must survive it.
    #[test]
    fn test_parse_envelope_tolerates_leading_noise() {
        let stdout = "Update available! 2.1.263 -> 2.2.0\n{\"is_error\":false,\"result\":\"ok\"}";
        assert_eq!(parse_envelope(stdout).unwrap(), "ok");
    }

    #[test]
    fn test_parse_envelope_error_envelope_is_classified() {
        let stdout = r#"{"is_error":true,"result":"Invalid API key - please run /login"}"#;
        let err = parse_envelope(stdout).expect_err("is_error must fail");
        assert_eq!(err.class, FailureClass::Auth);
    }

    #[test]
    fn test_parse_envelope_without_json_is_a_protocol_failure() {
        let err =
            parse_envelope("error: unknown option '--max-turns'").expect_err("non-JSON must fail");
        assert_eq!(err.class, FailureClass::Protocol);
    }

    #[test]
    fn test_parse_envelope_missing_result_is_a_protocol_failure() {
        let err = parse_envelope(r#"{"is_error":false}"#).expect_err("no result must fail");
        assert_eq!(err.class, FailureClass::Protocol);
    }

    #[test]
    fn test_classify_text_separates_auth_from_rate_limit_from_transport() {
        assert_eq!(classify_text("Please run /login"), FailureClass::Auth);
        assert_eq!(classify_text("HTTP 401 Unauthorized"), FailureClass::Auth);
        assert_eq!(
            classify_text("rate limit exceeded"),
            FailureClass::RateLimited
        );
        assert_eq!(classify_text("HTTP 429"), FailureClass::RateLimited);
        assert_eq!(
            classify_text("connection reset by peer"),
            FailureClass::Transport
        );
    }

    /// Every failure names the floor, so an unsupported-flag exit is readable
    /// as version drift rather than as a mystery.
    #[test]
    fn test_failure_display_names_the_version_floor() {
        let failure = ClaudeFailure::new(
            FailureClass::Protocol,
            "no JSON envelope",
            Some("2.1.100".to_string()),
        );
        let rendered = format!("{}", failure);
        assert!(rendered.contains("claude protocol failure"), "{}", rendered);
        assert!(rendered.contains("2.1.100"), "{}", rendered);
        assert!(rendered.contains(MIN_CLAUDE_VERSION), "{}", rendered);
    }

    #[tokio::test]
    async fn test_resolve_reports_not_found_for_a_missing_binary() {
        let err = ClaudeCli::resolve(
            Some(Path::new("/nonexistent/path/to/claude")),
            TRIAGE_TIMEOUT,
        )
        .await
        .expect_err("a missing binary must fail to resolve");
        assert_eq!(err.class, FailureClass::NotFound);
        assert!(format!("{}", err).contains("PATH"), "{}", err);
    }
}
