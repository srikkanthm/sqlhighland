# Intermittent TTC desync on encrypted (ANO) sessions

Status: **resolved (2026-09-14)** — root cause localized upstream and fixed by
re-pinning the driver fork to `528a79a`; not an ANO/marker path (see
[Resolution](#resolution)).
Date: 2026-09-14
Area: driver response parsing / ANO transport
(`rust-oracledb/src/client/mod.rs`, `rust-oracledb/src/transport.rs`,
`rust-oracledb/src/encryption.rs`)

## Resolution

The desync was an **upstream TTC bit-vector decode bug**, not the ANO transport
or the cancellation marker/recovery machinery suspected below. Re-pinning the
fork to `528a79a` picked up two upstream fixes:

- **per-row bit-vector reset**, and
- **correct bitmap length on the initial execute**.

After the re-pin the `unknown TTC message type` failure no longer reproduces.
The analysis below is kept as the historical investigation; its "candidate
causes" were not the culprit. The app's poisoning handling
(`src/db.rs::is_poisoned` → disconnect + lazy reconnect) remains as a safety net
for any future stream desync.

## Symptom

Intermittently, a run fails with this in the **output pane**:

```
Query failed: internal error: unknown TTC message type 42 at packet 1, offset 270
```

Observed characteristics:

- **Intermittent**, not reproducible on demand.
- The **same simple `SELECT`** fails each time it happens.
- The **next run works** (the app self-heals).
- The server requires **Native Network Encryption (ANO)**; the session negotiates
  AES-128 with `integrity = none`.
- Not necessarily correlated with export/Count rows (that was just when it was
  first noticed).

## What the error means

The driver's TTC response parser (`messages/mod.rs::deserialize_ttc_message`)
read the byte `0x2A` (`42`) where a TTC **message type** was expected. `42` is
not one of the driver's known message types (they run `1..=34`,
`constants.rs`), so the client and server **disagree about the response byte
stream** at that point — a desync, not a server-side error. "packet 1, offset
270" is the parser's position (second packet, 270 bytes in).

## Why it self-heals

`ErrorKind::UnknownTtcMessageType` is classified as **poisoning**
(`src/db.rs::is_poisoned`): a TTC desync means the connection can no longer be
trusted, so the app drops the session (`OracledbSession::disconnect`) and the
next run reconnects lazily. That is why the failure is transient and the next
run succeeds.

## Ruled out

| Suspect | Verdict | Evidence |
|---|---|---|
| Query-specific deserialize bug | **Unlikely** | The *same simple `SELECT`* succeeds most of the time; identical SQL/bytes. |
| Auth / connect | **Ruled out** | The connection completes the ANO handshake and authenticates; the failure is mid-response. |
| Checksum (integrity) desync | **Ruled out for this server** | `CRYPTO_CHECKSUM_SERVER` is unset → negotiated `integrity = none`; the fork's checksum path isn't active. |

## Candidate causes (in suspicion order)

1. **Marker/control or call-timeout recovery interrupting a multi-packet
   response.** In `client/mod.rs::receive_data_packet`, a `MARKER` packet
   triggers `reset()` and returns a **single** packet with
   `check_end_of_response = false`; a call timeout triggers
   `recover_from_error` (`client/mod.rs:226`, `:237`). Either path can leave the
   remainder of the in-flight response in the socket, so the **next** operation
   reads the leftover bytes and misparses → unknown message type. The fork added
   this marker/reset handling for cancellation, so a **Cancel** or a **timeout**
   is a prime suspect.
2. **ANO per-packet framing/decrypt edge case.** The fork's ANO transport
   (`transport.rs::transform_incoming`, `encryption.rs`) encrypts/decrypts each
   DATA packet body. A *failed* decrypt surfaces as `decrypt failed` /
   `validate failed` (logged), not type 42 — but a packet the transform *skips*
   (`buf.len() <= 1`) or a fragmentation/folding edge case could feed the parser
   a bad body.
3. **Rare upstream deserialize case** for a specific response shape (the
   upstream history has several "older DB / multiple packets" fixes, e.g.
   `4078406`, `191ba69`).

## Diagnostics

- **`SQLHIGHLAND_ANO_TRACE=1`** — writes `~/sqlhighland-ano-debug.log` with the
  `accept`/`negotiated` lines and every `client recv type=… flags=… len=…`
  marker/control packet, plus `decrypt failed`/`validate failed`. This shows
  whether a marker/control precedes the desync and whether ANO decryption is
  failing. (Off by default; launch from a terminal:
  `SQLHIGHLAND_ANO_TRACE=1 /Applications/SQLHighland.app/Contents/MacOS/SQLHighland`.)
- **`RSO_DEBUG_PACKETS=1`** — the driver prints every packet to stdout (launch
  from a terminal to capture it).
- Note what was happening at the time: running a query, **Cancel**, a
  **timeout**, scrolling/fetching more, **Count rows**, or an **export**.

## Planned fixes (when a trace is captured)

1. **Phase 1 — on-desync dump (fork):** on `unknown_ttc_message_type`, dump the
   current `Response` packets (`type`, `packet_flags`, `data_flags`, `buf.len()`)
   and a hex window around the failure offset, gated by `SQLHIGHLAND_ANO_TRACE`.
   This pinpoints the packet where the parse diverges.
2. **Phase 2 — optional stopgap (app):** on a poison desync during a **query
   run**, auto-retry the statement **once** (`SELECT`/`WITH` only — never
   DML/DDL) since a fresh reconnect always works; surface the error only if the
   retry also fails.
3. **Phase 3 — fix:** after the trace, fix the offending path in the fork (most
   likely draining the rest of the response on the marker/recovery path, or an
   ANO framing correction).

## Related

- [`docs/ANO.md`](ANO.md) — ANO/NNE port details and the connection trace.
- [`docs/CANCELLATION.md`](CANCELLATION.md) — the marker/OOB/reset machinery
  (`rust-oracledb` cancellation work) that the leading hypothesis points at.
