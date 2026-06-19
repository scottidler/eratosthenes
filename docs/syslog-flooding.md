# Syslog flooding by eratosthenes

**Investigated:** 2026-06-19 on `desk` · **Code anchored at:** commit `26fa4ee` (`main`)

## Summary

The `eratosthenes` systemd timer runs `eratosthenes run` every 5 minutes and each
run dumps its entire progress trace to **stdout via `println!`**. systemd captures
that stdout into the journal, and rsyslog mirrors the journal into
`/var/log/syslog`. The result on `desk`:

- `/var/log/syslog` = **4.5 GB**, `/var/log/syslog.1` = **2.7 GB** (~7 GB of text logs)
- `/var/log/journal` = **~4 GB**
- **35.3 million lines** in `/var/log/syslog`

This was a material contributor to `/` (915 GB `/dev/sda2`) hitting 100% full, which
in turn tripped Syncthing's 1%-free-space floor and silently halted file sync.

## Status (2026-06-19) — fixed in working tree, commit pending

Implemented and verified (`otto ci` green; per-run syslog `MATCH` count flat across
manual runs):

- **`src/engine.rs`**: all 18 core `println!` converted to the right `log` level
  (run-story → `info!`, mechanics → `debug!`, per-record/per-item → `trace!`); the
  redundant per-message `println!` deleted.
- **`src/logging.rs`**: `AccountLogger` now mirrors records to **stderr only when
  stderr is a TTY** (`IsTerminal`). Interactive `eratosthenes run` shows live
  progress; a headless timer run has no TTY, so nothing reaches the journal/syslog.
- **`src/main.rs`**: the per-run `cmd_run` "Completed successfully" `println!` → `info!`.
- **Second growth source found and fixed**: the per-account **log file** (`tatari.log`)
  had reached **4 GB** because the default level is `debug` and three per-thread lines
  (`evaluate_thread` entry, `[thread:] matched filter`, `[thread:] protected by`) were
  at `debug` though they are per-record. Demoted to `trace!`. Per-run file growth
  dropped from **~169 KiB to ~7 KiB** (~48 MiB/day → ~2 MiB/day).

Still outstanding (operational, not yet done): reclaim the existing ~15 GB of logs
(`tatari.log` 4 GB + `/var/log/syslog*` ~7 GB + journald ~4 GB), and add rotation so the
log file can't grow unbounded (see below).

## What is spamming

The chatter is emitted from `src/engine.rs`, which is **core/lib code**. A run prints,
among others, a line **per matched message** and **per evaluated thread**:

```
eratosthenes[…]: [filter:only-me-vip] MATCH: <subject> (from: <sender>)
eratosthenes[…]: [filter:only-me-vip] starring 4 messages
eratosthenes[…]: [state] searching active threads...
```

At 12 runs/hour (`OnUnitActiveSec=5min`, plus `OnBootSec=2min`) with one line per
matched message, this accumulates millions of lines.

### Offending `println!` calls — `src/engine.rs` @ `26fa4ee`

All 18 `println!` in this file write unconditionally to stdout:

| line(s) | content | correct level |
|---|---|---|
| 264 | per-message `MATCH: <subject>` | **delete** — duplicates the `debug!` at line 257 |
| 199, 280, 318, 325, 338, 497 | per-filter / per-action summaries (`searching:`, `N matched`, `starring/flagging/trashing N`) | `debug!` |
| 227, 388 | per-item progress (`[i/total] fetching/evaluating`) | `trace!` (tight loop) |
| 25, 33, 45, 151, 214, 380, 486 | run header, dry-run banner, run-level summary | `info!`, or stdout **only when attached to a TTY** |

## Root cause

1. **`println!` bypasses the log level.** eratosthenes already has a real logging
   framework (`src/logging.rs`: custom `log::Log`, `--log-level`, writes to the XDG
   log file, `set_max_level(Trace)`). But these per-run lines go through `println!`,
   not `log::*`, so **`--log-level warn` does nothing to suppress them**. They always
   print.

2. **Duplicate emission.** The per-message MATCH is logged *twice* — `debug!(...)` at
   `engine.rs:257` (which correctly routes to the file logger) **and** `println!(...)`
   at `engine.rs:264`. The `println!` is pure redundancy and is the stdout source.

3. **Shell/core violation.** Per `rules/rust.md`: *"Core functions return `Result<T>` —
   never call `process::exit` or print to stdout/stderr from core."* `engine.rs` is
   core; all of its `println!` belong in the `main.rs` shell (for interactive runs) or
   in the logger.

4. **Per-record logging at the wrong level.** Per `rules/logging.md`: per-item /
   per-record lines in a loop must be `trace!`, not unconditional output. The
   per-message MATCH and per-item progress lines are exactly this case.

5. **No systemd stdout containment.** The `eratosthenes.service` user unit has **no
   `StandardOutput=` directive**, so stdout defaults to the journal, and Ubuntu's
   rsyslog mirrors journal → `/var/log/syslog`. There is also no journald size cap.

## What is required to fix

### 1. Code (primary fix) — `src/engine.rs`

- **Delete** the `println!` at line 264 (the `debug!` at 257 already covers it), and
  **demote that `debug!` to `trace!`** — it is per-record.
- **Convert** the remaining `println!` to the appropriate `log` macro per the table
  above so they route through `src/logging.rs` (file logger) and obey `--log-level`.
- For the genuine run-summary output that is useful interactively (header / dry-run
  banner / final counts), either move it to the `main.rs` shell or gate it on
  `std::io::IsTerminal` so **nothing prints to stdout when run headless** by the timer.

Net effect: a headless timer run emits nothing to stdout; full detail still lands in
the XDG log file at the chosen level. This is the real fix — it stops the flood at the
source regardless of systemd/rsyslog config.

### 2. systemd (stop-gap / defense-in-depth) — `eratosthenes.service`

Even before the code change, the bleed can be stopped because the app already logs to
its own file:

```ini
[Service]
StandardOutput=null      # discard stdout; do not feed the journal/syslog
StandardError=journal    # keep real errors in the journal
```

Reload after editing: `systemctl --user daemon-reload && systemctl --user restart eratosthenes.service`.

Optionally reduce cadence in `eratosthenes.timer` (`OnUnitActiveSec=5min` → `15min`);
verbosity, not frequency, is the root cause, so this is secondary.

### 3. Reclaim the existing logs (after the fix is in place)

```sh
journalctl --vacuum-size=500M                              # trim the 4 GB journal
sudo truncate -s 0 /var/log/syslog /var/log/syslog.1       # truncate (files are held open by rsyslog; do NOT rm)
```

### 4. Prevent recurrence — cap the journal

`/etc/systemd/journald.conf`:

```ini
[Journal]
SystemMaxUse=1G
```

then `sudo systemctl restart systemd-journald`.

## Verification

After the code fix + restart, confirm a headless run is silent and the logs stop growing:

```sh
systemctl --user start eratosthenes.service
journalctl --user -u eratosthenes.service -n 50 --no-pager   # should show start/stop, not per-message MATCH
wc -l /var/log/syslog                                        # should stay flat across several 5-min cycles
```
