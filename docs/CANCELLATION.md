# Query cancellation — implementation & upstream revert guide

Status: **shipped** 2026-09-13 for **plain TCP only**, backed by a **fork**
of the `oracledb` driver. TCPS/TLS is **deferred** — see §7. This document
records what we changed, why, how to verify it, and **exactly how to drop the
fork once upstream ships cancellation**.

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
- Pinned commit: `52993b57ae798fd207db5830153970e9428fd6cb`
- Commits added on top of upstream:
  - `4412161` Add plain TCP cancellation API
  - `9a4fcb1` Use out-of-band break for cancellation; map ORA-01013 to Cancelled
  - `49bb38a` Advertise OOB capability and fix interrupt recovery
  - `52993b5` Advertise OOB only on plain TCP; never panic in the connect path

Pinned from `Cargo.toml`:

```toml
[patch.crates-io]
oracledb = { git = "https://github.com/srikkanthm/rust-oracledb", rev = "52993b57ae798fd207db5830153970e9428fd6cb" }
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
| `src/client/mod.rs` | **`reset()` recovery fix** (see §4; also consumes `CONTROL` packets); `cancel_stream()`; `supports_oob()` |
| `src/client/capabilities.rs` | parses `protocol_options`, sets `supports_oob` when the server echoes `GSO_CAN_RECV_ATTENTION` |
| `src/messages/connect.rs` | advertises `GSO_CAN_RECV_ATTENTION` + `TNS_CHECK_OOB` **on plain TCP only**; parses `protocol_options`; connect-path errors instead of `todo!()`/`unwrap` (see §3.4) |
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

### 3.4 Connect-path hardening (2026-09-14)

A crash report (`SIGABRT` inside `ConnectMessage::deserialize`) on a
different Oracle server exposed panics in the connect handshake. They are now
errors, not aborts:

- `_ => todo!()` on an unexpected response packet → a clear error that also
  logs and names the numeric packet type.
- `todo!()` when the server requires Native Network Encryption
  (`SQLNET.ENCRYPTION_SERVER=REQUIRED`) → a "not implemented" error.
- `parse::<usize>().unwrap()` on a malformed listener `(ERR=…)` → falls back
  to "unexpected refuse".

The out-of-band **advertisement is now plain-TCP only**: `tcps://` never sets
`GSO_CAN_RECV_ATTENTION`/`TNS_CHECK_OOB`. Oracle's own thin driver disables
OOB for TLS, and a TCP urgent break cannot cross a TLS stream. This was the
only handshake behavior our fork changed, and the likely trigger.

`Client::reset()` now also consumes `CONTROL` packets (it previously skipped
only `MARKER`), so a reset can never hand a control packet to a message
parser that doesn't expect one.

---

## 4. Root cause: the hang after an interrupt

The original implementation aborted the statement server-side but then
**hung forever** in the driver's `Client::reset()`. That routine sent its own
reset marker and waited for *two* markers — but the caller had already
consumed the server's reset marker, so the loop blocked on a marker that
never came.

The fix (fork commits `49bb38a`, then `52993b5`, `src/client/mod.rs`) is to
discard marker packets (and consume control packets) and return the **first
data packet**, which carries the operation's actual result/error:

```rust
loop {
    let packet = self.transport.receive_packet()?;
    match packet.packet_type {
        constants::PACKET_TYPE_MARKER => continue,
        constants::PACKET_TYPE_CONTROL => {
            self.process_control_packet(packet)?;
            continue;
        }
        _ => return Ok(packet),
    }
}
```

This is the change that made cancellation return promptly and keep the
connection alive. The OOB advertisement and `supports_oob` plumbing are
still present for plain-TCP servers that accept urgent breaks, but are inert
on a server that reports `false` (and never advertised over `tcps://`).

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

---

## 7. TCPS / TLS cancellation (deferred — needs a test target)

Status: **not implemented.** Sessions using `tcps://` get
`cancel_token() == None`, so Cancel falls back to client-side abandon and the
server keeps running the statement. This section records why, and the two
viable designs, so the work can be picked up once a TCPS Oracle is available.
**There is currently no TLS Oracle to validate against**, which is the main
reason it is tabled: every candidate is server-behavior-dependent and cannot be
proven with unit tests.

### 7.1 Why the plain-TCP trick does not carry over

1. **OOB breaks are not supported over TLS.** Oracle's own thin driver
   disables them for `tcps` (`python-oracledb` `protocol.pyx`:
   `if use_tcps: self._caps.supports_oob = False`); Oracle Net's break is TCP
   urgent data (the `DISABLE_OOB` sqlnet parameter). The fork mirrors this:
   it never advertises OOB on `tcps://` (fork commit `52993b5`), and
   `Transport::cancel_stream` returns `None` when `tls_stream.is_some()`
   (`src/transport.rs:257`).
2. **The in-band marker cannot be injected from another thread.** It must be
   written through rustls's `StreamOwned` TLS state, which lives in `Transport`
   behind the same client mutex the running query holds (`conn_impl.rs:58`).
   Writing plaintext on a cloned fd would corrupt the TLS record stream.
3. So there is no independent-socket path to cancel on, unlike plain TCP.

### 7.2 The opening: the interrupt path already works through TLS

`Client::recover_from_error()` (`src/client/mod.rs:214`) already sends
`MARKER_TYPE_INTERRUPT` through the **normal transport** (TLS included) and then
`reset()`s — that is how the existing call-timeout recovery works. What is
missing is waking the blocked read to check a cancel flag: reads block
indefinitely unless a call timeout is set (`transport.rs:246`; SQLHighland sets
60 s at `src/db.rs:193`).

### 7.3 Option A — cooperative in-band cancel (recommended)

Fork changes:

1. Add a per-connection cancel request (`Arc<AtomicBool>`) reachable from a
   handle, so `CancelHandle::cancel()` can set it (today `CancelHandle` is tied
   to the cloned stream and errors when there is none —
   `src/connection/mod.rs`).
2. Give `Transport`/`Client` a short internal poll read timeout (~100–250 ms)
   while a token is armed. On wake, in `Client::receive_data_packet`
   (`src/client/mod.rs:135`):
   - real call-timeout deadline passed → `ErrorKind::CallTimeoutExceeded`
     (existing behavior),
   - cancel flag set → send `MARKER_TYPE_INTERRUPT`, `reset()`, return
     `Error::cancelled()`,
   - otherwise keep reading.
3. Keep the pool manager's call-timeout save/restore intact
   (`pool/manager.rs:65`); the poll timeout is transport-internal and must not
   be confused with the user's call timeout.
4. Add a unit test for flag→interrupt and a live TCPS test (§7.5).

App changes: essentially none — the `CancelToken` seam already abstracts this.
`db.rs` would hand back a flag-setting token for TLS sessions and keep the
immediate socket token for plain TCP.

Trade-offs: uniform, no privileges, keeps the connection; cancel latency equals
the poll interval (~100–250 ms); it touches the central read/timeout loop, so
the call-timeout and pooling paths need regression coverage.

### 7.4 Option B — server-side `ALTER SYSTEM CANCEL SQL` (fallback)

Run `ALTER SYSTEM CANCEL SQL 'sid, serial#'` from a helper connection. It is
transport-agnostic (works on TCPS and plain TCP) and would let us drop the fork
entirely. Caveats:

- requires the `ALTER SYSTEM` privilege (often unavailable to app accounts),
- needs the target session's `SID` **and** `SERIAL#` (from `V$SESSION` /
  `SYS_CONTEXT`),
- opens/maintains a second session and adds round trips,
- coverage for PL/SQL blocks (e.g. `dbms_lock.sleep`) varies.

It could ship as a privilege-gated fallback: use the token first, else the
helper connection when the account has the privilege.

### 7.5 Verification plan (needs a TCPS Oracle)

- A TLS variant of `tests/cancel_live.rs` (wallet/one-way TLS) asserting the
  same "returns promptly + `Cancelled` + connection reusable" outcome.
- Regression: the 60 s call timeout still trips correctly and the session
  survives; pool acquire/release still restores timeouts.
- Confirm the server actually honors an in-band interrupt delivered through the
  TLS stream — the key unknown that requires a live target.
