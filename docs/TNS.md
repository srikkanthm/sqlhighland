# TNS aliases, connect descriptors, and connection properties

Status: **planned, not started** (2026-09-18). Decisions below are proposed,
not locked. This document records the research and the plan so the work can be
picked up without re-investigating the driver.

## Goal

Let a saved connection be defined by more than host/port/service:

1. **Connection-string modes** — Basic (today), TNS alias resolved from
   `tnsnames.ora`, or a raw Connect Descriptor.
2. **Connection/system properties** — a curated set of Oracle Net / session
   options (failover, SDU, server type, connection class, program, machine,
   terminal, wallets, …).

## What the driver supports (research findings)

Pinned fork: `oracledb` rev `528a79a`. Its own
`doc/appendix_a.md` is the authority:

| Feature | rust-oracledb |
| --- | --- |
| Password authentication | Yes |
| External authentication | **No** |
| Token-based authentication | **No** |
| Kerberos and Radius authentication | **No** |
| `tnsnames.ora` file | Yes |
| Easy Connect / Connect Descriptor | Yes |
| One-way TLS / mTLS / wallets | Yes |
| Native Network Encryption | "No — use TLS instead" (our fork added ANO separately) |

**RADIUS, Kerberos, external, and token auth are not supported by the
driver.** There is no way to expose them from the client without upstream/fork
work. Anything we surface must map to a driver-recognized option or it is
silently ignored.

### Connection strings

`Config::set_connect_string(&str)` accepts:

- **Easy Connect**: `[[protocol:]//]host[:port]/service_name[:server][/instance_name]`.
- **Full Connect Descriptor**: `(DESCRIPTION=…(ADDRESS=…)(CONNECT_DATA=…))`.
- **TNS alias**: a bare name resolved from `tnsnames.ora`, located via
  `Config::set_config_dir(dir)` or the `TNS_ADMIN` / `ORACLE_HOME` environment
  variables. The alias is resolved when `set_connect_string` is called; a
  missing alias is a hard error.

The parser is a **whitelist**: unrecognized `DESCRIPTION`, `CONNECT_DATA`,
`ADDRESS`, `ADDRESS_LIST`, and `SECURITY` nodes are dropped, not forwarded.

### Descriptor options the driver honors

`FAILOVER`, `LOAD_BALANCE`, `SOURCE_ROUTE`, `RETRY_COUNT`, `RETRY_DELAY`,
`EXPIRE_TIME`, `SDU`, `USE_SNI`, `USE_TCP_FAST_OPEN`, `SERVICE_NAME`,
`INSTANCE_NAME`, `SERVER`, `SID`, `POOL_BOUNDARY`, and `SECURITY`
(`SSL_SERVER_CERT_DN`, `SSL_SERVER_DN_MATCH`, `WALLET_LOCATION`).

### `Config`-level properties (sent as session properties)

`set_cclass` (connection class), `set_program`, `set_machine`, `set_osuser`,
`set_terminal`, `set_driver_name`, `set_stmtcachesize`,
`set_wallet_location` / `set_wallet_password`, and `set_auth_mode`
(SYSDBA/SYSOPER/etc.).

## Plan

### Phase 1 — Connection-string modes

`ConnectionConfig` (`src/model.rs`) gains:

- `connect_mode: ConnectMode` — `Basic` | `TnsAlias` | `Descriptor`,
  `#[serde(default)]` → `Basic`, so old files are unchanged.
- `tns_alias: String`
- `connect_descriptor: String`
- `config_dir: String` (TNS_ADMIN override; empty = use env/default)

Behavior:

- `connect_string()` branches on mode. `Basic` keeps `service_kind`/`ssl`.
  `TnsAlias` returns the alias; `Descriptor` returns the raw descriptor.
- `validate()` checks the fields that mode needs (Basic: host/service;
  TnsAlias: alias; Descriptor: non-empty).
- `src/db.rs` passes `set_config_dir` when set, then `set_connect_string`.
- Dialog: a mode selector (pills) swaps the Host/Port/Service block for an
  Alias field or a Descriptor text area.
- Update non-Basic-mode surfaces: sidebar/picker detail lines
  (`src/conn_picker.rs`) and the default-name fallback
  (`src/connection_dialog.rs`).
- `CacheFingerprint` (`src/metadata.rs`) gains mode/alias/descriptor/config_dir
  so on-disk suggestion caches invalidate on a mode change.

### Phase 2 — Connection/system properties

- `properties: Vec<ConnProperty { key, value }>` on `ConnectionConfig`
  (ordered, stable serde).
- Map each to the right mechanism: descriptor-recognized keys appended to a
  generated descriptor; `Config`-level keys via setters.
- **Reject unknown keys in `validate()`** with a clear message — the driver
  silently ignores unrecognized nodes, so a typo would otherwise look like it
  applied.
- Dialog: an add/remove key/value list.

## Tradeoffs / open decisions

- **Raw descriptor vs structured fields.** A raw descriptor is the most
  flexible (RAC, multiple addresses, wallets) but unvalidated until connect;
  structured fields are friendlier but limited. Proposal: raw descriptor for
  `Descriptor` mode; keep `Basic` structured.
- **Property validation.** Strict whitelist (error on unknown) prevents silent
  no-ops. Recommended.
- **`config_dir` scope.** Per-connection is most correct (different
  `TNS_ADMIN` per database); a global preference is simpler. Proposal:
  per-connection, empty = inherit the default.
- **Wallet secrets.** `set_wallet_password` is a secret and would need the same
  `Zeroizing`/keychain treatment as the DB password. Defer the wallet-password
  UI; support wallet via the descriptor's `WALLET_LOCATION` first.

## Open questions

1. Actual need: (a) TNS aliases/descriptors, (b) Oracle Net descriptor
   properties, or (c) something the driver cannot do (RADIUS/Kerberos/external
   auth)? This determines whether Phase 2 is worth doing at all.
2. Raw Connect Descriptor field, or only TNS alias + Basic?
3. `config_dir` per-connection or global?
4. Properties: curated set or free key/value validated against a whitelist?
5. Wallet/mTLS: is `WALLET_LOCATION` in a descriptor enough, or is a wallet
   password prompt needed too?
