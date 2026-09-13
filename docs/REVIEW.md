# SQLHighland — Code & Security Review

Date: 2026-09-13
Scope: whole tree (`src/`, `tests/`, `Cargo.toml`)
Baseline verified locally: `cargo clippy --features gui --all-targets` clean,
`cargo test --lib` green.

Findings are ordered by severity. Items marked **FIXED** were addressed in the
same change set as this document; the rest are open recommendations.

Update (follow-up change sets): the organization findings in §2 and the stale
docs in §4.3 are fixed, the Tier 1 hygiene items (§3 formatting, §4.1, §4.2,
§4.4), the Tier 2 security items (new-connection default, in-memory
zeroization, tab-id path safety), the Tier 3 remainder (§3 lock policy and
logging, plus an MSRV CI job), cross-platform P0 (§6.1 home-dir resolution),
and port hardening P1/P2 (§6.2 Windows ACLs, §6.4 Linux/Windows check jobs) are
fixed — see the change sets at the end. Later change sets add P3 (cross-platform
keychain backends, §6.3) and P4 (view-state grouping, §6.6). §6 tracks any
remaining cross-platform / second-engine work.

## 1. Security

### 1.1 Plaintext passwords with default file permissions — **FIXED (partially)**
`PasswordMode::File` is the `#[default]` (`src/model.rs`) and
`ConnectionConfig.password` is serialized to
`~/.config/sqlhighland/connections.toml`. The write path used `fs::write`
with no `chmod`, so under the common `umask 022` the file (and the pre-rename
`*.tmp` sibling) was `0644` — any local account could read every saved
password.

Fix:
- New `src/fsutil.rs::write_atomic` writes to a sibling temp file, applies
  owner-only `0600` (dir `0700`), `fsync`s the file, renames, then `fsync`s
  the parent directory.
- `config::write_atomic` routes through it with `secret = true`;
  `filetab::write` uses the same crash-safe path with default permissions.
- `config::tighten_owned` migrates an already-written file (and its
  directory) to `0600`/`0700` on load, so existing installs are repaired.
- Tests: `fsutil::tests::secret_writes_are_owner_only`,
  `config::tests::load_tightens_loose_connections_file`.

Still open: `File` is still selectable and remains the serde default for
legacy entries; the plaintext is only owner-readable but unencrypted at rest.

### 1.1b New-connection default — **FIXED**
`PasswordMode::default_for_new()` now returns `Keychain` on Apple platforms
and `Ask` elsewhere, so new connections no longer start in plaintext `File`
mode. `File` remains the serde default so legacy files keep loading.
`connection_dialog::start_add` seeds the dialog with this mode.

### 1.2 `Debug` leaked the password — **FIXED**
`ConnectionConfig` derived `Debug`, so any `{:?}` (panic messages, future
logging, error reports) printed the plaintext secret. Replaced with a manual
`Debug` that renders `password: "<redacted>"`. `SavedConfig`'s derived
`Debug` now inherits the redaction.
Test: `model::tests::debug_redacts_password`.

### 1.3 Non-durable atomic writes — **FIXED**
The old write-then-rename did not `fsync`, so a power loss could still lose
the rename or leave a stale temp. `fsutil::write_atomic` now syncs the file
and the parent directory. The temp name is `path + ".tmp"` (rather than
`with_extension("tmp")`), so `a.sql` and `a.toml` no longer share a temp.

### 1.4 In-memory secrets are now zeroized — **FIXED (mostly)**
`ConnectionConfig.password` is a `zeroize::Zeroizing<String>` and the
session-unlocked map is `HashMap<String, Zeroizing<String>>`, so every copy
wipes its buffer on drop (including clones and connection deletion). The
`test-support` GUI tests, `oracledb`'s own credentials copy, and the GPUI
`InputState` buffers are outside our control and still hold transient
plaintext while a dialog/connection is live.

### 1.5 TLS is opt-in and transport-only — open (informational)
`ConnectionConfig.ssl` defaults to `false` and only switches the EZCONNECT
scheme to `tcps://`. There is no certificate/wallet validation control. Fine
for localhost, worth documenting before remote use.

### 1.6 Path handling — **FIXED**
`TabsManifest::draft_path` now requires a safe single-component id
(`is_safe_id`: rejects empty, `.`, `..`, and `/ \ : NUL`), and `load`
rekeys any unsafe id from a hand-edited/corrupt `tabs.toml` with a fresh
UUID instead of letting it reach the filesystem. Test:
`config::tests::unsafe_tab_ids_are_rejected_and_rekeyed`.

### 1.7 SQL construction — no issues found
`rewrite_describe` / `escape_literal`, `system_predicate` (escapes
`own_schema`), and `parse_identifier` all quote-escape correctly. No
injection found.

## 2. Organization / architecture

Good: clean GUI/headless split behind the `gui` feature (`src/lib.rs`), real
seams (`DbClient`, `SchemaProvider`), poison-safe `session::lock`, atomic
state writes, strong doc comments, and a large test suite.

**FIXED — file-size / impl sprawl.** The four giants were split by
responsibility using the pattern documented in `../ARCHITECTURE.md` (parent file
+ submodule directory; inherent `impl SqlHighlandView` blocks re-opened per
module; re-exports preserve public paths; tests in a sibling `tests.rs`):

| File | Before | Parent after | Submodules |
| --- | --- | --- | --- |
| `src/app.rs` | 3,491 | 752 | `app/{lsp,results,tabs,connections,actions,render}.rs` |
| `src/sql.rs` | 1,721 | 108 | `sql/{split,classify,substitute,script}.rs` |
| `src/complete.rs` | 2,233 | 102 | `complete/{context,catalog,aliases,ranking}.rs` |
| `src/run.rs` | 1,291 | 30 | `run/{query,script,export}.rs` |

Every source file is now under ~1,000 lines. Behavior-preserving: 116 lib
tests green, GUI UI tests unchanged, `clippy --all-targets` clean.

Still open:
- 3× `#[allow(clippy::too_many_arguments)]` remain in `run/export.rs`.

(Field-grouping follow-up done in P4 — see §6.6: `SqlHighlandView` is now 21
fields via `ConnectionDialogState` / `PendingOps` / `BrowserState`.)

## 3. Code smells

- **FIXED:** `ConnectionConfig` debug redaction (see 1.2).
- **FIXED:** inconsistent lock handling — every `MetadataCache`/`ResultData`
  mutex now goes through `session::lock` (poison-tolerant) instead of
  `.lock().ok()`/`.map(...)` silently degrading. `SessionPool::remove` keeps
  its intentional non-blocking `try_lock`.
- **FIXED:** `eprintln!` replaced by the `logging` shim (`logging::warn` /
  `logging::error`) at all five call sites.
- **FIXED:** formatting is now enforced — `cargo fmt --all` applied across the
  crate (including the inline `CompleteMode` doc) and CI runs
  `cargo fmt --all -- --check`.
- No `#[must_use]` on fallible/query helpers (minor).

## 4. Build / test hygiene

### 4.1 Gui-only tests are not feature-gated — **FIXED**
`tests/browser_tree.rs` and `tests/themes.rs` now have `required-features =
["gui"]` `[[test]]` entries in `Cargo.toml`, matching `ui_picker`/`menus`.
Plain `cargo test` / `cargo clippy --all-targets` (no gui) compiles again.

### 4.2 No CI / toolchain / audit config — **FIXED**
Added `.github/workflows/ci.yml` (fmt; core clippy `-D warnings` + tests on
Linux without gui; a macOS GUI job gated on the Metal toolchain — currently
`continue-on-error` until the runner image is confirmed; a non-blocking
`rustsec/audit-check`; and a blocking MSRV (1.89) job, added in Tier 3) and
`rust-toolchain.toml` (stable + rustfmt/clippy).

### 4.3 Stale docs — **FIXED**
`../README.md` was refreshed: correct test counts (116 lib / 11 live), the three
password modes, shipped features (autocomplete, schema browser, CSV/XLSX
export), and a pruned roadmap. `PROGRESS.md` remains a historical build log.

### 4.4 Package metadata — **FIXED**
`Cargo.toml` now has `description`, `repository`, and `publish = false`
(private app). A `license` is intentionally omitted until distribution terms
are chosen; add it (plus a LICENSE file) if the source is ever published.

## 5. Rust best practices

- Few production `unwrap`/`expect`, all guarded
  (`complete/aliases.rs` non-empty group, `app/tabs.rs` pre-checked path,
  `main.rs` startup). No `unsafe` outside tests.
- `oracledb 26.0.0-beta.3` is a beta dependency (documented, isolated in
  `db.rs`). A transitive `block 0.1.6` (via `gpui-pre` → `cocoa`) reports a
  future-incompat warning upstream; not fixable locally.

## 6. Cross-platform & engine readiness

Assessed after the Tier 1–3 cleanups (Oracle-first, macOS the current target).
The GUI stack is **not** Apple-locked upstream: `gpui-pre 0.3.4` selects
`gpui-pre-linux` (Linux/FreeBSD), `gpui-pre-windows`, `gpui-pre-web` (wasm),
and `gpui-pre-macos` per target. `security-framework` is an Apple-only target
dep and `keychain` compiles a stub elsewhere. Remaining gaps are app-level.

### 6.1 `HOME`-only path resolution — **FIXED (P0)**
`config::base_dir` read `HOME` only, so on Windows the config dir failed and
connections/tabs/preferences never persisted; `~` expansion in `@`-scripts and
the export / save-as default dirs assumed `HOME` too. All four now use
`fsutil::home_dir()` (`HOME` → `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH`), and the
config dir uses `%APPDATA%\sqlhighland` on Windows. Precedence is unit-tested
(`fsutil::tests::home_dir_precedence_is_cross_platform`).

### 6.2 Secret-file ACLs on Windows — **FIXED (P1)**
`fsutil::restrict` now has a Windows implementation: it drops inherited ACEs
and grants the current user via `icacls`, re-applied to the temp file with the
handle closed so the renamed target is owner-only. Best-effort (failures are
ignored, as on Unix); verified by cross-compiling the module for
`x86_64-pc-windows-msvc` (`rustc --emit=metadata`), not by a runtime ACL test.
Windows also now has a real keychain backend — see §6.3.

### 6.3 Keychain backends — **FIXED (P3)**
Switched to the `keyring` crate (v4, `v1` API), which selects the native store
per platform: macOS Keychain Services, Windows Credential Manager, and Linux
Secret Service (D-Bus). The direct `security-framework` dependency and our
per-platform modules/stubs are gone; the macOS mapping (service + account
generic password) is unchanged, so existing entries still resolve.
`PasswordMode::default_for_new()` now returns `Keychain` on macOS *and*
Windows, and `Ask` elsewhere (a headless Linux box may lack a Secret Service).
Compilation of the off-macOS stores is covered by the provisional
`windows-core` / `linux-gui` CI jobs; runtime is untested here.

### 6.4 Port CI — **PARTIALLY FIXED (P2)**
Added two non-blocking jobs to `.github/workflows/ci.yml`: `linux-gui`
(`cargo check --features gui`, installing the GPUI Linux system deps) and
`windows-core` (`cargo check --lib`, with NASM installed for `aws-lc-sys`).
Both are `continue-on-error` until confirmed green on the runners — the apt set
and the kit's Linux/Windows support are unproven. Flip them blocking once
verified. Windows still isn't a plain `cargo check`: `oracledb` → rustls pulls
`aws-lc-sys`, which needs a native C toolchain (MSVC/CMake/NASM).

### 6.5 Second-engine plumbing — open
`DbClient` / `SchemaProvider` / `DbEngine` are real seams, but the session layer
is Oracle-concrete: `SessionPool` = `HashMap<String, Arc<Mutex<OracledbSession>>>`
(`session.rs`), `FetchState.session` (`app/results.rs`), and the run/export
signatures (`run/query.rs`, `run/export.rs`). The `DbClient` trait has no
incremental-cursor API (`start_query`/`fetch_more` live on `OracledbSession`),
and the metadata fetchers are Oracle dictionary SQL. A second engine needs its
own cursor + metadata + dialect module.

### 6.6 View-state breadth — **FIXED (P4)**
`SqlHighlandView` dropped from ~44 fields to 21 by grouping the connection
dialog (`ConnectionDialogState`), modal-resume runs (`PendingOps`), and
per-connection caches + schema-browser (`BrowserState`) into focused structs in
`app.rs`. Behavior unchanged (119 lib tests + GUI UI tests green).

## Change set — security

- `src/fsutil.rs` (new): secure atomic write + `restrict` helper.
- `src/lib.rs`: register `fsutil`.
- `src/config.rs`: secure writes, owner-only migration on load.
- `src/filetab.rs`: crash-safe (fsynced) user-file writes.
- `src/model.rs`: redacting `Debug for ConnectionConfig`.
- Tests: fsutil permissions/round-trip, config migration, model redaction.

## Change set — organization (follow-up)

- `src/app/` (new): `lsp`, `results`, `tabs`, `connections`, `actions`,
  `render` — split out of a 3,491-line `app.rs` (752 now).
- `src/sql/` (new): `split`, `classify`, `substitute`, `script`, `tests` —
  1,721-line `sql.rs` now 108.
- `src/complete/` (new): `context`, `catalog`, `aliases`, `ranking`,
  `tests` — 2,233-line `complete.rs` now 102.
- `src/run/` (new): `query`, `script`, `export` — 1,291-line `run.rs` now 30.
- `../ARCHITECTURE.md` (new): module map and the split pattern.
- Docs: `../README.md` refreshed.
- Remaining files all < ~1,000 lines; 116 lib tests green, clippy clean.

## Change set — hygiene (Tier 1)

- `.github/workflows/ci.yml` (new): fmt, core clippy/tests, macOS GUI
  clippy/tests, RustSec audit.
- `rust-toolchain.toml` (new): stable + rustfmt/clippy.
- `Cargo.toml`: gui tests feature-gated; `description`/`repository`/
  `publish = false`; `security-framework` moved to an Apple-only target dep.
- `cargo fmt` applied crate-wide; `cargo fmt --check` clean.
- `src/keychain.rs`: macOS backend behind `cfg(target_vendor = "apple")` with
  a std-only stub elsewhere, so the core builds on Windows/Linux.
- Docs reworded to engine-first / cross-platform (Oracle first, macOS current).
- Fixes §3 (formatting) and §4.1, §4.2, §4.4.

## Change set — security (Tier 2)

- `Cargo.toml`: `zeroize` (with `serde`) dependency.
- `model.rs`: `ConnectionConfig.password` is `Zeroizing<String>`;
  `PasswordMode::default_for_new()` (Keychain on Apple, Ask elsewhere).
- `app.rs` / `app/connections.rs`: session-unlocked map is
  `HashMap<String, Zeroizing<String>>`; `effective_password` returns a
  zeroizing value.
- `connection_dialog.rs`: new connections seed the platform default mode.
- `config.rs`: safe tab-id validation + rekey of unsafe ids on load.
- `tests/live.rs`: updated to the new password type.
- Fixes §1.1b, §1.4, §1.6; 117 lib tests green.

## Change set — hygiene (Tier 3)

- `src/session.rs` / `providers.rs` / `app/lsp.rs` / `app/results.rs` /
  `browser.rs` / `run/query.rs` / `run/export.rs`: all cache/result locks now
  use `session::lock`, removing the silent `.lock().ok()`/`.map()` degradation.
- `src/logging.rs` (new): `warn`/`error` shim; the five `eprintln!` sites
  replaced.
- `.github/workflows/ci.yml`: added a blocking MSRV (1.89) job
  (`cargo +1.89.0 check --all-targets`), verified locally on 1.89.0.
- Fixes the §3 lock and logging items; 117 lib tests green, clippy clean.

## Change set — cross-platform (P0)

- `src/fsutil.rs`: `home_dir()` (`HOME` → `USERPROFILE` → `HOMEDRIVE`+`HOMEPATH`)
  with a platform-independent, unit-tested precedence helper.
- `src/config.rs`: `base_dir` uses `%APPDATA%\sqlhighland` on Windows, else
  `home_dir()/.config/sqlhighland`.
- `src/sql/script.rs`, `src/run/export.rs`, `src/app/tabs.rs`: `~` expansion and
  dialog default dirs use `home_dir()`.
- Fixes §6.1; 119 lib tests green, clippy clean (macOS).

## Change set — port hardening (P1 + P2)

- `src/fsutil.rs`: platform `restrict` impls — Unix permission bits, Windows
  `icacls` owner-only, no-op elsewhere; `write_atomic` re-applies after close
  so the renamed file is restricted on Windows.
- `.github/workflows/ci.yml`: `linux-gui` (`cargo check --features gui` with
  GPUI Linux deps) and `windows-core` (`cargo check --lib` with NASM), both
  non-blocking/provisional.
- Fixes §6.2 and partially §6.4.

## Change set — view-state grouping (P4)

- `src/app.rs`: new `ConnectionDialogState` (dialog form + pending option
  pills), `PendingOps` (bind/pick/password resume), and `BrowserState`
  (dictionary caches, usage, schema-browser trees/filters). `SqlHighlandView`
  drops from ~44 to 21 fields; call sites updated across `app/*`, dialogs,
  `browser`, `sidebar`, and `run`.
- Fixes §6.6; 119 lib tests green, clippy clean, GUI UI tests pass.

## Change set — cross-platform keychain (P3)

- `Cargo.toml`: add `keyring = "=4.2.0"` (`v1` API); remove the Apple-only
  `security-framework` dependency.
- `src/keychain.rs`: rewritten on `keyring::Entry` — one cross-platform
  implementation for macOS Keychain / Windows Credential Manager / Linux
  Secret Service, replacing the Apple module and the non-Apple stubs.
- `src/model.rs`: `default_for_new()` now prefers `Keychain` on Windows too.
- Fixes §6.3; 119 lib tests green, clippy clean, MSRV (1.89) verified.
