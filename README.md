# eratosthenes

The Great Sieve of Eratosthenes to fix my fucking email.

A Gmail API-native "inbox zero" engine. It applies message filters and ages mail
off the inbox (INBOX -> Purgatory -> Oblivion) on a timer, while protecting
Starred and Important threads with `ttl: Keep` so they stay put.

## Commands

- `eratosthenes run [accounts...]` - run the inbox-zero engine (default command).
- `eratosthenes digest [accounts...]` - post the pinned-inbox (Starred +
  Important) digest to Slack.
- `eratosthenes auth login|logout|status` - manage OAuth2 tokens.
- `eratosthenes config validate|show` - inspect resolved config.
- `eratosthenes service install|uninstall|reinstall|status|start|stop` - manage
  the systemd user timers (run + digest).

## Configuration

Per-account YAML lives at `~/.config/eratosthenes/<account>.yml`. See
[`eratosthenes.example.yml`](eratosthenes.example.yml) for a full annotated example.

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
- `eratosthenes digest` posts one message: two grouped sections (Starred,
  Important) with per-item date / sender / subject, the subject deep-linked to
  the Gmail thread. An empty pinned set posts a positive `Inbox clear` line.
- Querying is at the thread level, so each thread is exactly one line even if it
  has several starred replies.

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
