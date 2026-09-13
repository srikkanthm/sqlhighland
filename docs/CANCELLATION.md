# Query cancellation — implementation & upstream revert guide

Status: **shipped** 2026-09-13, backed by a **fork** of the `oracledb`
driver. This document records what we changed, why, how to verify it, and
**exactly how to drop the fork once upstream ships cancellation**.

> Companion: [`PROGRESS.md`](PROGRESS.md) has the dated build-log entry;
> [`../ARCHITECTURE.md`](../ARCHITECTURE.md) shows where the seams live.

---

## 1. TL;DR

- Upstream `oracledb` has **no break/cancel API** (verified in source); its
  only "cancel" is dropping the connection. Previously SQLHighland's Cancel
  was *client-side abandon*: a per-tab run token was bumped, late results
  discarded, and **the server kept running the statement** until it finished.
- We forked the driver (`srikkanthm/rust-oracledb`) and added a plain-TCP
  interrupt. SQLHighland pins the fork by commit in `[patch.crates-io]`.
- The app keeps a driver-agnostic seam (`CancelToken`), so removing the fork
  later is mostly a local edit in `src/db.rs` — see §6.

---

## 2. What SQLHighland does (engine-agnostic seam)

Cancel now fires a **real server-side interrupt** when the session can supply
one, and always keeps the old client-side abandon as a fallback.

| Piece | Location | Responsibility |
|---|---|---|
| `CancelToken` trait | `src/db.rs` | `fn cancel(&self) -> Result<(), DbError>`; `Send + Sync`, callable while the query holds the session mutex |
| `DbClient::cancel_token()` | `src/db.rs` | returns `Option<Arc<dyn CancelToken>>`; default `None` (no cancel available) |
| `OracleCancelToken` | `src/db.rs` | wraps `oracledb::CancelHandle` (the only fork-specific code in the app) |
| `OracledbSession.cancel` | `src/db.rs` | `Arc<oracledb::CancelHandle>` captured at connect via `conn.cancel_handle().ok()`; cleared in `disconnect` |
| `SessionPool` registry | `src/session.rs` | `set_cancel_token` / `cancel_token` keyed by connection id; cleared in `remove` |
| `cancel_run` | `src/run/query.rs` | fires the token, then bumps `run_token` (fallback + discard late results) |
| `cancel_export` | `src/run/export.rs` | fires the token; the drain treats an interrupted fetch as *cancelled*, not failed |
| token capture | `src/run/query.rs`, `src/run/script.rs`, `src/app/connections.rs` | store the handle after connect / a completed run / explicit Connect |
| `Cancelled` mapping | `src/db.rs` | `oracledb::ErrorKind::Cancelled` → friendly `"Query cancelled"` |

**Why out of the session mutex?** A running query holds the session lock for
its whole duration, so a cancel button that needed that lock would block.
The driver's handle owns an independent clone of the TCP socket, so it can
write the interrupt without touching the query mutex.

**Fallback behavior:** when `cancel_token()` is `None` (TLS/TCPS, a
non-Oracle engine, or a tab whose token was never captured), Cancel degrades
to the old abandon path — the UI unblocks immediately, the late result is
discarded by the run token, and the server finishes in the background.
The **first** lazily-connected run in a tab has no token yet (the handle is
stored when the run completes); subsequent runs on that connection cancel
for real. An explicit sidebar Connect stores the token up front.

---

## 3. The fork

- Repo: `https://github.com/srikkanthm/rust-oracledb` (branch `main`)
- Base: upstream `26.0.0-beta.4` era (fork commits sit on top of `466c453`)
- Pinned commit: `49bb38a05fd2fe325361726ea016216f3eacbd9b`
- Commits added on top of upstream:
  - `4412161` Add plain TCP cancellation API
  - `9a4fcb1` Use out-of-band break for cancellation; map ORA-01013 to Cancelled
  - `49bb38a` Advertise OOB capability and fix interrupt recovery

Pinned from `Cargo.toml`:

```toml
[patch.crates-io]
oracledb = { git = "https://github.com/srikkanthm/rust-oracledb", rev = "49bb38a05fd2fe325361726ea016216f3eacbd9b" }
```

### 3.1 Public API added

- `Connection::cancel_handle() -> Result<CancelHandle, Error>` — handle for
  interrupting an in-flight operation.
- `Connection::cancel() -> Result<(), Error>` — convenience over the above.
- `Connection::supports_oob() -> bool` — whether the server accepts a TCP
  urgent (out-of-band) break.
- `pub struct CancelHandle` (re-exported at the crate root) with
  `cancel(&self)`.
- `ErrorKind::Cancelled` + `Error::cancelled()` /
  `Error::cancel_not_supported()`.

### 3.2 Internal changes

| File | Change |
|---|---|
| `src/connection/mod.rs` | `CancelHandle` (independent socket clone; tries OOB first on Unix, else the in-band marker); `Connection::{cancel_handle,cancel,supports_oob}` |
| `src/connection/conn_impl.rs` | stores `cancel_stream` / packet-size / OOB capability; wires the handle |
| `src/transport.rs` | `cancel_stream()` returns a cloned plain-TCP socket, or `None` for TLS |
| `src/client/mod.rs` | **`reset()` recovery fix** (see §4); `cancel_stream()`; `supports_oob()` |
| `src/client/capabilities.rs` | parses `protocol_options`, sets `supports_oob` when the server echoes `GSO_CAN_RECV_ATTENTION` |
| `src/messages/connect.rs` | advertises `GSO_CAN_RECV_ATTENTION` + `TNS_CHECK_OOB`; parses `protocol_options` |
| `src/response/mod.rs` | ORA-01013 (`DB_ERR_NUM_USER_REQUESTED_CANCEL`) → `ErrorKind::Cancelled` |
| `src/error.rs` | new `Cancelled` kind + constructors |
| `src/constants.rs` | `DB_ERR_NUM_USER_REQUESTED_CANCEL = 1013` |
| `src/packet.rs` | removed now-unused `has_reset_marker` |
| `src/lib.rs` | re-export `CancelHandle` |
| `Cargo.toml` | `socket2` (Unix) for `send_out_of_band` |

### 3.3 What the server actually does

- `CancelHandle::cancel()` tries a **TCP urgent (OOB) break** when the server
  advertised support, otherwise sends the **in-band `MARKER_TYPE_INTERRUPT`**
  packet.
- The local test Oracle reports `supports_oob() == false` (it does not echo
  `GSO_CAN_RECV_ATTENTION`), so the **in-band marker** is the path that runs
  in practice. It does abort a running statement (a 15 s `dbms_lock.sleep`
  returned in ~3 s).
- The server reports the abort as **ORA-01013**, which the driver maps to
  `ErrorKind::Cancelled`. The connection stays usable afterward.

---

## 4. Root cause: the hang after an interrupt

The original implementation aborted the statement server-side but then
**hung forever** in the driver's `Client::reset()`. That routine sent its own
reset marker and waited for *two* markers — but the caller had already
consumed the server's reset marker, so the loop blocked on a marker that
never came.

The fix (fork commit `49bb38a`, `src/client/mod.rs`) is to discard marker
packets and return the **first non-marker (data) packet**, which carries the
operation's actual result/error:

```rust
loop {
    let packet = self.transport.receive_packet()?;
    if packet.packet_type != constants::PACKET_TYPE_MARKER {
        return Ok(packet);
    }
}
```

This is the change that made cancellation return promptly and keep the
connection alive. The OOB advertisement and `supports_oob` plumbing are
still present for servers that accept urgent breaks, but are inert on a
server that reports `false`.

---

## 5. Verification

Requires a real Oracle on **plain TCP** (no TCPS/TLS).

```sh
cargo test --lib                                   # seam/registry unit tests
cargo test --test cancel_live -- --ignored --nocapture
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

`tests/cancel_live.rs` (opt-in, `#[ignore]`) runs a 15 s statement, cancels
it, and asserts the statement returns well before it would have finished and
that the connection still works. Env overrides: `ORACLE_HOST` / `ORACLE_PORT`
/ `ORACLE_SERVICE` / `ORACLE_USER` / `ORACLE_PWD` / `CANCEL_SQL`.

Observed against the local test Oracle:

```
server supports OOB break: false
cancellation target: BEGIN dbms_lock.sleep(15); END;
cancel() returned in 47.958µs; statement returned in 3.005858958s
error kind: Cancelled
error: the database operation was cancelled
connection reusable after cancel: OK
```

---

## 6. Reverting to upstream `oracledb`

Do this once upstream ships an equivalent interrupt API (or you decide the
fork is no longer worth carrying). The app seam is engine-agnostic, so most
of this is isolated to `src/db.rs` and the opt-in test.

1. **Confirm upstream coverage.** Check the upstream release notes / API for
   all three capabilities we rely on:
   - interrupt an in-flight operation without the session mutex,
   - a distinguishable cancellation error (dedicated kind, or a stable
     ORA-01013 mapping), and
   - connection recovery after the interrupt (the reset path).
2. **Drop the patch** in `Cargo.toml`:
   - delete the `[patch.crates-io]` block,
   - set `oracledb = "..."` (`src/Cargo.toml` line ~13) to the release that
     ships cancellation.
3. **Refresh the lockfile:**
   ```sh
   cargo update -p oracledb      # or remove the git entry and cargo build
   ```
4. **Adapt the Oracle adapter** (`src/db.rs` — the only fork-coupled code):
   - `OracledbSession.cancel` field: retype from `Arc<oracledb::CancelHandle>`
     to the upstream handle type (or drop the field if upstream exposes
     cancellation differently).
   - `connect()`: replace `conn.cancel_handle().ok().map(Arc::new)` with the
     upstream constructor/registration.
   - `OracleCancelToken::cancel`: call the upstream interrupt method.
   - `disconnect()`: keep clearing the handle.
   - `DbError::from`: retarget the `ErrorKind::Cancelled` arm to upstream's
     variant. If upstream surfaces a plain DB error instead, match the ORA
     code (`1013`) or the message text there.
5. **Update the opt-in test** (`tests/cancel_live.rs`): it calls
   `conn.cancel_handle()` / `conn.supports_oob()` directly. Swap in the
   upstream equivalents (or delete the test if upstream already covers it).
6. **Keep everything else unchanged:** the `CancelToken` trait,
   `DbClient::cancel_token()`, the `SessionPool` registry, and the
   `run/query.rs`, `run/script.rs`, `run/export.rs`, `app/connections.rs`
   call sites never mention the fork and need no edits.
7. **Re-verify:**
   ```sh
   cargo clippy --all-targets -- -D warnings
   cargo test --lib
   cargo test --test cancel_live -- --ignored --nocapture
   ```
8. **Update docs:** this file, `PROGRESS.md` (dated entry) and the `README`
   dependency note; drop the fork repo if you no longer need it.

> If upstream adds the capability under a different shape (e.g. a
> `Connection::interrupt()` plus a `Cancelled` error), the only mandatory
> app edit is step 4 — the rest of the pipeline is decoupled by design.

### Bumping the fork before then

When rebasing the fork onto a newer upstream, push the branch and re-pin the
`rev` to the new commit, then re-run the verification commands above. Always
test against a real plain-TCP Oracle — the OOB path and the reset recovery
are server-dependent and will not show up in unit tests.
