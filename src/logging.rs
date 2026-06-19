use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use eyre::{Context, Result};
use log::{Level, LevelFilter, Log, Metadata, Record};

use eratosthenes::cfg::config::xdg_data_dir;

tokio::task_local! {
    pub static ACCOUNT: String;
}

struct AccountLogger {
    app_level: LevelFilter,
    // When stderr is a TTY (interactive run), mirror records to the console so a
    // human running `eratosthenes run` sees live progress. A headless timer run
    // has no TTY, so nothing reaches the journal/syslog -- this is what stopped
    // the per-message `println!` flood (see docs/syslog-flooding.md).
    interactive: bool,
    files: Mutex<HashMap<String, Mutex<File>>>,
}

impl AccountLogger {
    fn new(app_level: LevelFilter, interactive: bool, log_dir: &Path, accounts: &[&str]) -> Result<Self> {
        let mut files = HashMap::new();
        for &name in accounts {
            let path = log_dir.join(format!("{}.log", name));
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("Failed to open log file {}", path.display()))?;
            files.insert(name.to_string(), Mutex::new(file));
        }
        Ok(Self {
            app_level,
            interactive,
            files: Mutex::new(files),
        })
    }
}

impl Log for AccountLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        if metadata.target().starts_with("eratosthenes") {
            metadata.level() <= self.app_level
        } else {
            metadata.level() <= Level::Warn
        }
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let msg = format!(
            "{} [{:5}] {} - {}\n",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
            record.level(),
            record.target(),
            record.args()
        );
        if self.interactive {
            eprint!("{}", msg);
        }
        let account = ACCOUNT.try_with(|n| n.clone()).ok();
        let files = self.files.lock().expect("logger mutex poisoned");
        match account.and_then(|n| files.get(&n).map(|_| n)) {
            Some(name) => {
                let file_mutex = &files[&name];
                let mut f = file_mutex.lock().expect("file mutex poisoned");
                let _ = f.write_all(msg.as_bytes());
            }
            None => {
                for file_mutex in files.values() {
                    let mut f = file_mutex.lock().expect("file mutex poisoned");
                    let _ = f.write_all(msg.as_bytes());
                }
            }
        }
    }

    fn flush(&self) {}
}

pub fn setup(level: &str, accounts: &[&str]) -> Result<()> {
    let log_dir = xdg_data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eratosthenes")
        .join("logs");

    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    let app_level = match level.to_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    };

    let interactive = std::io::stderr().is_terminal();
    let logger = AccountLogger::new(app_level, interactive, &log_dir, accounts)?;
    log::set_boxed_logger(Box::new(logger)).map_err(|e| eyre::eyre!("Failed to initialize logger: {}", e))?;
    log::set_max_level(LevelFilter::Trace);

    Ok(())
}
