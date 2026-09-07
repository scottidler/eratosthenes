# eratosthenes

The Great Sieve of Eratosthenes to fix my fucking email.

A Gmail API-native "inbox zero" engine. It applies message filters and ages mail
off the inbox (INBOX -> Purgatory -> Oblivion) on a timer, while protecting
Starred and Important threads with `ttl: Keep` so they stay put.

## Commands

- `eratosthenes run [accounts...]` - run the inbox-zero engine (default command).
  - `--dry-run` - no message or thread changes; missing labels may still be created.
  - `--mark-only` - one-shot marker backfill (see below); applies no Star/Flag/Move.
- `eratosthenes triage [accounts...]` - classify new inbox threads into `llm/*`
  bucket labels with one LLM call (see [Triage](#triage) below).
  - `--dry-run` - full classify pass, prints the thread -> bucket table, zero
    Gmail mutations. Stricter than `run --dry-run`.
- `eratosthenes digest [accounts...]` - post the pinned-inbox (Needs Reply +
  Starred + Important) digest to Slack.
- `eratosthenes auth login|logout|status` - manage OAuth2 tokens.
- `eratosthenes config validate|show` - inspect resolved config.
- `eratosthenes service install|uninstall|reinstall|status|start|stop` - manage
  the systemd user timers (run + digest + triage, the latter two installed
  only for accounts that opt in via a `slack:`/`triage:` block respectively).

## Configuration

Per-account YAML lives at `~/.config/eratosthenes/<account>.yml`. See
[`eratosthenes.example.yml`](eratosthenes.example.yml) for a full annotated example.

### Message filters act once

A message-filter stamps a `marker-label` (default `Triaged`) on every message it
HANDLES, whether or not the action changed anything. Every message-filter
excludes marked mail, so unstarring or un-flagging a message is permanent: it
is never re-acted on. Adopting this needs no config change (the marker
defaults); override the name with `marker-label` if `Triaged` collides with
something in your mailbox. It must not name a `state-filters` label or
destination, or stage age-off would strip it right back off.

Delete the `Triaged` label in the Gmail UI to reset: every message becomes
eligible again on the next run.

**Rolling this out onto an existing inbox:** the first run after adopting
markers would otherwise re-apply every filter to your ENTIRE current pinned
set one last time, since none of it carries the marker yet. Avoid that with a
one-shot backfill:

```sh
systemctl --user stop eratosthenes.timer     # stop the timer first
eratosthenes run --dry-run --log-level debug # sanity-check the per-filter counts
eratosthenes run --mark-only                 # stamp today's mailbox, apply nothing
eratosthenes run                             # should now report 0 matched
systemctl --user start eratosthenes.timer    # resume
```

`run --mark-only` stamps the marker on exactly the set a normal run would
HANDLE (post-match, post-claim, deduped by message id) and issues zero
`STARRED`/`IMPORTANT`/`Move` writes. It logs one INFO line per stamped message
(id, date, from, subject): the stamp is irreversible in effect, so that log is
how a wrongly-frozen message gets found and hand-cleared. Mail that arrives
while the timer is stopped is stamped and never starred - keep the window
short.

### Triage

Add an optional `triage` block to any account to enable it (design doc:
[`docs/design/2026-07-06-llm-triage.md`](docs/design/2026-07-06-llm-triage.md)):

```yaml
triage:
  schedule: "Mon-Fri 06:30:00"   # REQUIRED systemd OnCalendar; controls the triage timer
  max-threads: 50                # per-run candidate cap; the remainder is picked up next run
  classify-model: claude-haiku-4-5-20251001
  draft-model: claude-sonnet-5
  voice-profile: ~/Claude/writing/VOICE.md
```

- KEYLESS by construction: classification and drafting shell out to the
  locally installed `claude` CLI, which owns its own auth. The config holds no
  Anthropic API key.
- Every candidate thread (INBOX, not yet carrying `llm/seen`) gets exactly one
  `llm/*` bucket label plus `llm/seen` in one batched LLM call. The
  five default buckets (`needs-reply`, `fyi-work`, `recruiting`, `receipts`,
  `noise`) are configurable; see `eratosthenes.example.yml` for the full
  `buckets:` block and the matching `state-filters` needed to age each one.
- `needs-reply` threads whose newest real message isn't already answered (no
  Sent reply, no existing DRAFT) get one threaded reply draft written in
  Scott's voice into Gmail Drafts. Nothing is ever sent: a build-time grep
  test (`tests/no_send_guard.rs`) fails the build if a `messages_send` /
  `drafts_send` call appears anywhere under `src/`.
- Hitting `max-threads` logs loudly and never truncates silently; the
  remainder is classified on the next run.

`eratosthenes triage --dry-run` runs the full classify pass with **zero Gmail
mutations** (no labels created, no `llm/seen` written) - stricter than `run
--dry-run`, which may still create missing labels. Actually run against the
live `tatari` account (2026-09-07):

```
$ eratosthenes triage --dry-run
Connecting to Gmail...
[dry-run] label 'llm/seen' does not exist yet
max-threads cap HIT: 402 candidate threads, classifying the newest 50, 352 left unseen for the next run (raise max-threads if this repeats)
1a07c73974fbf453     fyi-work       What did Forrester find about Zero Trust adoption?
1a07c61e7b9e2276     fyi-work       Daily Proactive Checklist for Tatari 07 Sep 2026
1a07c5ecbfb90ff1     receipts       [Domain renewing automatically] Your domain tataritest.com will be automatically renewed
1a077734d9d7ae50     recruiting     Director – GPU Stack Unified Build & Release Platform at AMD: up to $370K/year
1a06fef65919263a     needs-reply    Avinash Basani submitted a take home test for Senior Data Platform Engineer
1a06eceedb8b46a2     noise          How the Brooklyn Nets save 150+ hours a month with Expensify
... (50 rows total)
Triage: 50 threads classified, 0 labeled, 0 skipped (dry run)
```

`eratosthenes config validate` reports the resolved triage config alongside
message-filters and state-filters (real output, same account):

```
Triage: configured, schedule 'Mon..Fri 06:30:00'
  Buckets: 5 defined
    - needs-reply -> llm/needs-reply
    - fyi-work -> llm/fyi-work
    - recruiting -> llm/recruiting
    - receipts -> llm/receipts
    - noise -> llm/noise
```

### Slack digest

Add an optional `slack` block to any account to enable the digest for it:

```yaml
slack:
  token-env: SLACK_XOXP_TOKEN    # NAME of the env var holding the user token (xoxp)
  channel: D01G4Q7AWLV           # self-DM channel (note-to-self), or Uxxxx/Cxxxx
  browser-index: 0               # Gmail multi-login slot (/u/N) for deep links
  schedule: Mon,Thu 07:00:00     # REQUIRED systemd OnCalendar; controls the digest timer
```

- The digest is a no-op for any account without a `slack` block.
- The token is never stored in YAML; the config names an env var. A user token
  (`xoxp`) is used because the destination is your self-DM, which only your own
  token can post into. It needs the `chat:write` scope.
- `eratosthenes digest` posts one message: three grouped sections (Needs Reply,
  Starred, Important) with per-item date / sender / subject, the subject
  deep-linked to the Gmail thread. An empty pinned set posts a positive
  `Inbox clear` line.
- A thread appears exactly ONCE, in its highest section: Needs Reply beats
  Starred beats Important. Needs Reply is the `triage:` block's `needs-reply`
  bucket label intersected with the inbox, so the section only exists for an
  account that has a `triage:` block.
- Querying is at the thread level, so each thread is exactly one line even if it
  has several starred replies.

#### Bullets

An account with a `triage:` block also gets 3-7 summary bullets under every
pinned thread, generated at digest time by the same keyless `claude` transport
triage uses, on the `triage:` block's `classify-model`. Nothing is cached.

- When the newest inbound message asks you for something, that ask renders as a
  marked FIRST line (`*Reply needed:*`) above the bullets. It is a separate
  field, not one of the 3-7 bullets.
- No ask means no marker and no placeholder: the absence of the marker is the
  signal.
- Bullets are capped at 80 characters each, in the prompt and again in Rust.
- Over the readability budget the digest sheds bullets before it sheds threads,
  never drops an ask, and only then drops trailing threads least-actionable
  first (Important, then Starred, then Needs Reply).
- Any `claude` failure still posts the digest, subjects and deep links intact,
  with a `bullets unavailable: <cause>` line naming the failure class. An
  account with NO `triage:` block posts an un-enriched digest with no such line;
  that is not a failure.

### Digest timer

`eratosthenes service install` lays down the digest service + timer **only if at
least one account has a `slack` block**. The timer fires on `slack.schedule`
(a required `OnCalendar` string, system local time; there is no default - omit it
and the config fails to parse). The Slack token is read
from `~/.config/eratosthenes/digest.env` (mode 600), which `service install`
populates from your environment for each distinct `token-env` referenced.

To enable end to end:

```sh
export SLACK_XOXP_TOKEN=xoxp-...        # in the environment service install sees
eratosthenes service reinstall          # lays down run + digest units
eratosthenes digest                      # verify a manual post
```

### Triage timer

`eratosthenes service install` lays down the triage service + timer **only if
at least one account has a `triage` block**, firing on that account's
`triage.schedule`. If more than one triage-enabled account requests a
different schedule, the first one wins and a warning names the discarded
account.

`service reinstall` is required to pick up the triage timer on a config that
predates it - `service status` on this host currently shows only
`eratosthenes.timer` and `eratosthenes-digest.timer` (real output, 2026-09-07);
the triage timer is not yet installed, because a reinstall is deliberately
withheld until Scott's eval sign-off (`docs/eval/llm-triage-eval.md`) clears
live labeling. `service reinstall` in the meantime is destructive to any
already-issued OAuth token cache in this environment - see the implementation
notes' INCIDENT entry before running it.
