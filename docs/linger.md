# systemd linger requirement (run timer)

## TL;DR

The `eratosthenes.timer` (the periodic run timer) **silently stops running
unless user lingering is enabled**. Without it, the engine appears installed and
"enabled" but quietly stops culling the inbox the moment you log out or the
machine suspends. Enable it once per machine:

```sh
loginctl enable-linger "$USER"
```

## Incident: 2026-06-11

Found the inbox refilled to ~1,078 unread on `desk`. Investigation:

- `eratosthenes.timer` was `enabled` and `active`, but `systemctl --user
  list-timers` showed **no next trigger** and a last run of **2026-06-06
  21:14** (its log: "Done: 22 messages matched filters, 0 threads
  transitioned", then nothing for 5 days).
- The `eratosthenes-digest.timer` was healthy and had fired that morning.
- `loginctl show-user $USER` reported **`Linger=no`**.

That asymmetry is the whole story (see "Why" below).

### Fix applied

```sh
loginctl enable-linger saidler          # Linger=no -> yes
systemctl --user restart eratosthenes.timer
systemctl --user start eratosthenes.service   # run once now + anchor the timer
```

After this the run timer showed a concrete next trigger (~5 min out) and now
re-arms every 5 minutes regardless of login state. Verified `Linger=yes`.

## Why the run timer dies but the digest timer survives

Without lingering, the per-user systemd manager shuts down when the last session
for that user ends (logout, reboot, sometimes suspend) and only comes back when
you log in again. The two timers react differently to that gap:

- **Run timer** is monotonic: `OnBootSec=2min` + `OnUnitActiveSec=5min`
  (see `src/service.rs`). `OnUnitActiveSec` is measured from the last time the
  *service* was active. If the user manager isn't running continuously, that
  clock has no anchor and the timer never re-arms after the session that
  installed it ends. Result: it runs for one login session, then goes quiet.
- **Digest timer** is wall-clock: `OnCalendar=<schedule>` + `Persistent=true`.
  An absolute calendar time plus `Persistent=true` survives the manager
  stopping and starting, so it fires on the next login after a missed window.

So the calendar-driven digest kept working while the interval-driven run timer
silently flatlined. Lingering keeps the user manager up 24/7, which both timers
ultimately want, but only the run timer *breaks* without it.

## Recommendation for the code

`eratosthenes service install` should not rely on a `println!` hint that the
user scrolls past (`src/service.rs:312`). Options, best first:

1. **Enable lingering as part of install.** Run `loginctl enable-linger $USER`
   (or detect `Linger=no` and do it) right after `systemctl --user enable
   --now`. This is the operation the run timer actually requires to function.
2. **Surface linger state in `service status`.** Report `Linger=yes/no`
   alongside the timer state so a dead-but-"enabled" timer is obvious.
3. **Consider making the run timer wall-clock too** (`OnCalendar=*:0/5` +
   `Persistent=true`) so a missed window is caught on next login even if
   lingering is somehow off, matching the digest timer's robustness.

Until one of those lands, enabling lingering once per machine is the manual
workaround and is the first thing to check if the inbox starts refilling.
