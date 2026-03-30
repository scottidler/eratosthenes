use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use eyre::{Context, Result};
use log::{Level, LevelFilter, Log, Metadata, Record};

tokio::task_local! {
    pub static ACCOUNT: String;
}

struct AccountLogger {
    app_level: LevelFilter,
    files: Mutex<HashMap<String, Mutex<File>>>,
}

impl AccountLogger {
    fn new(app_level: LevelFilter, log_dir: &PathBuf, accounts: &[&str]) -> Result<Self> {
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
        Ok(Self { app_level, files: Mutex::new(files) })
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
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eratosthenes")
        .join("logs");

    fs::create_dir_all(&log_dir).context("Failed to create log directory")?;

    let app_level = match level.to_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn"  => LevelFilter::Warn,
        "info"  => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _       => LevelFilter::Info,
    };

    let logger = AccountLogger::new(app_level, &log_dir, accounts)?;
    log::set_boxed_logger(Box::new(logger))
        .map_err(|e| eyre::eyre!("Failed to initialize logger: {}", e))?;
    log::set_max_level(LevelFilter::Trace);

    Ok(())
}
