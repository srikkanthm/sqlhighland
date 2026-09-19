# TODO / backlog

Planned work not yet implemented. Each item is a self-contained spec.

---

## Keep-alive for connections

Status: **planned, not started** (2026-09-14). Decisions below are locked.

### Goal

Detect idle connections that have quietly died (server, firewall/NAT, or
network drop) and reflect that honestly in the UI, while optionally keeping
idle sessions from timing out. Off by default.

### Locked decisions

- **Off by default** (`keepalive_secs = 0`); opt in via Settings.
- **On loss: mark + disconnect, do not auto-reconnect.** The next
  Run/script/export reconnects through the existing lazy-connect + password
  flow (silent for File/Keychain/unlocked, prompt for Ask). Never prompt in
  the background.
- **New "Connections" settings page** (inserted before About, so
  `ABOUT_PAGE_IX` 3 → 4).

### Behavior

- When enabled, every `keepalive_secs` seconds, ping each live connection that
  is not currently in use.
- Success: nothing changes. Failure: disconnect the session, clear its cancel
  token, drop it from the `live` set, and show a status like
  `Connection to <name> lost — will reconnect on next run` (red sidebar dot).
- Busy sessions (a run/export holds the session mutex) are skipped — traffic
  already keeps them alive.

### Implementation checklist

- [ ] `src/db.rs`
  - Add `fn ping(&self) -> Result<(), DbError> { Ok(()) }` to `DbClient`
    (default no-op = assume healthy; only Oracle overrides).
  - Implement on `OracledbSession`: require `self.conn`; set a fixed **5s**
    call timeout, call `conn.ping()`, then restore the configured timeout via
    `apply_call_timeout()`. The short timeout bounds how long a black-holed
    socket can hold the session lock, so a heartbeat can never stall a user
    query behind the 60s default.
  - No fork change: the driver already exposes `Connection::ping()` (a TTC
    RPC ping, works over TLS too, unlike cancel).
- [ ] `src/session.rs`
  - `pub fn session(&self, id: &str) -> Option<SharedSession>` (Arc clone).
  - `pub fn clear_cancel_token(&self, id: &str)`.
  - Unit tests: `session()` returns the same Arc / `None` for unknown;
    `clear_cancel_token` removes the entry.
- [ ] `src/config.rs`
  - `#[serde(default = "default_keepalive")] pub keepalive_secs: u64` → `0`.
  - Add to `Default`; extend the default + round-trip tests.
- [ ] `src/app/connections.rs` (loop) + `src/main.rs` (start it)
  - `SqlHighlandView::start_keepalive(&mut self, cx)`: one detached loop,
    started from `main.rs` after the view is built (NOT in `new()`, so headless
    tests spawn no timer).
  - Each iteration: read `Preferences::load().keepalive_secs`; if `0`, sleep
    30s and re-check (so re-enabling is picked up live), else sleep the
    interval, then snapshot live `(id, SharedSession)` pairs.
  - On the background executor per session: `try_lock()` — busy ⇒ skip; else
    `guard.ping()`. On error, `guard.disconnect()` while holding the lock, then
    on the UI thread `live.remove(id)`, `pool.clear_cancel_token(id)`, status.
  - Break the loop when `view.update` fails (view dropped).
- [ ] `src/settings_dialog.rs`
  - New `SettingPage::new("Connections")` before About.
  - Group "Keep-alive": `settings_pick_row` options
    `Off / 30 seconds / 1 minute / 2 minutes / 5 minutes` (values
    `0/30/60/120/300`) writing `prefs.keepalive_secs` via
    `Self::save_prefs_status`, plus a one-line explanation.
  - Keywords: `keep-alive, heartbeat, ping, idle, disconnect, connection`.
  - Bump `ABOUT_PAGE_IX` 3 → 4.
- [ ] `tests/live.rs` (needs Oracle)
  - `ping()` succeeds on a connected session; errors after `disconnect()`.
  - Optional: ping with an open cursor, then keep fetching (confirms ping
    doesn't disturb an open cursor).
- [ ] Docs
  - README feature bullet (off by default; Settings → Connections).
  - `docs/PROGRESS.md` entry when shipped.

### Edge cases / notes

- Ping while a transaction is open is safe (it does not commit).
- Ping during an export drain: `try_lock` usually succeeds between 1000-row
  chunks; ping is an independent RPC. Verify manually.
- `query_timeout_secs = 0` (unlimited): the ping still uses its own 5s and
  restores `None` afterward.
- Multiple simultaneous losses: the last status message wins (acceptable).
- No idle-activity tracking in v1 — the `try_lock` skip plus a 1-minute cadence
  is cheap enough. Idle gating is an easy follow-up if pings on active
  sessions ever matter.

### Alternative (not chosen)

Enable OS-level TCP `SO_KEEPALIVE` in the fork's `Transport`. It prevents
middlebox idle drops but can't detect app-level death or reflect UI state.
Could be layered in later if wanted.

---

## TNS aliases, connect descriptors, and connection properties

Status: **planned, not started** (2026-09-18). Full research and plan in
[`TNS.md`](TNS.md). Summary:

- Support connection-string **modes** (Basic / TNS alias / raw Connect
  Descriptor) plus a curated set of Oracle Net and session **properties**.
- The driver does **not** support RADIUS, Kerberos, external, or token auth;
  only password auth, TLS/mTLS/wallets, `tnsnames.ora`, and the descriptor
  options it recognizes.
- Open questions (see `TNS.md`): the real need, raw descriptor vs alias-only,
  `config_dir` scope, curated vs free-form properties, and wallet support.

---

## Parallel GUI-test isolation (config-dir env race)

Status: **known issue, not fixed** (found 2026-09-18).

### Symptoms

`cargo test --features gui-test --test browser_tree` fails intermittently:

```
---- disk_cached_metadata_loads_without_a_database stdout ----
assertion `left == right` failed: the persisted dictionary is loaded from disk
  left: []
 right: ["SCOTT.EMP"]
```

It passes with `--test-threads=1`.

### Cause

Both tests in `tests/browser_tree.rs` (`browser_expand_shows_tree` and
`disk_cached_metadata_loads_without_a_database`) set and then remove the
process-global `SQLHIGHLAND_CONFIG_DIR` with **no lock**. Cargo runs the tests
in one binary on parallel threads, so one test's `set_var`/`remove_var`
redirects the other's config lookups mid-run.

This is the same class of bug the rest of the suite already guards against:
`tests/tab_nav.rs` has a static `ENV_LOCK` and `src/testenv.rs` exposes
`config_dir_lock` for unit tests. `browser_tree.rs` predates the disk-cache test
and never picked up the pattern.

### Fix

- [ ] Add a `static ENV_LOCK: Mutex<()>` and a guard (same shape as
  `tests/tab_nav.rs:111-119`) to `tests/browser_tree.rs`; acquire it at the top
  of both tests and hold it through cleanup.
- [ ] Re-run `cargo test --features gui-test --test browser_tree` (default
  parallelism) several times to confirm it is green.

### Notes

- A repo-wide alternative is one process per test binary, but the existing
  per-binary lock is the established convention here.

---

## CI runs only some headless GUI test suites

Status: **known issue, not fixed** (found 2026-09-18).

### Cause

The blocking `gui` job in `.github/workflows/ci.yml` runs only:

```sh
cargo test --features gui-test \
  --test menus --test browser_tree --test themes --test ui_picker
```

The suites added later are not listed, so they never run in CI:

- `fonts` (3), `tab_nav` (19), `quit_guard` (8), `completion_scope` (14),
  `sql_diagnostics` (2) — ~46 tests, plus the `browser_tree` race above.

### Fix

- [ ] Add the missing `--test` flags to the CI headless step
  (`.github/workflows/ci.yml`), or switch to an explicit list of every
  gui-test binary.
- [ ] Do **not** use a blanket `--tests`: it would also build/run `live` and
  `cancel_live`, which need a live Oracle (and `cancel_live` is `#[ignore]`d).
- [ ] Land the `browser_tree` lock first so the lane stays green with default
  parallelism.
- [ ] The two `#[ignore]`d `ui_picker` tests are unrelated and stay as-is
  (see `REVIEW.md` §4.5).
