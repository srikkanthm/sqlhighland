# SQLHighland — Code & Security Review

Date: 2026-09-13
Scope: whole tree (`src/`, `tests/`, `Cargo.toml`)
Baseline verified locally: `cargo clippy --features gui --all-targets` clean,
`cargo test --lib` green.

Findings are ordered by severity. Items marked **FIXED** were addressed in the
same change set as this document; the rest are open recommendations.

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

Still open: the default password mode is still `File`. Consider making
`Keychain` the default (the implementation already exists), or removing
`File` for new connections.

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

### 1.4 In-memory secrets are not zeroized — open (low)
`unlocked: HashMap<String, String>` and the dialog `InputState` hold
passwords in plain heap memory for the process lifetime. Consider a
`secrecy`/`zeroize`-style wrapper if the threat model includes memory
scraping.

### 1.5 TLS is opt-in and transport-only — open (informational)
`ConnectionConfig.ssl` defaults to `false` and only switches the EZCONNECT
scheme to `tcps://`. There is no certificate/wallet validation control. Fine
for localhost, worth documenting before remote use.

### 1.6 Path handling — open (low)
`tabs.toml`'s `SavedTab.id` flows into `TabsManifest::draft_path` via
`{tab_id}.sql`. A hand-edited id containing `../` could write outside the
config dir. Sanitize/validate ids on load.

### 1.7 SQL construction — no issues found
`rewrite_describe` / `escape_literal`, `system_predicate` (escapes
`own_schema`), and `parse_identifier` all quote-escape correctly. No
injection found.

## 2. Organization / architecture

Good: clean GUI/headless split behind the `gui` feature (`src/lib.rs`), real
seams (`DbClient`, `SchemaProvider`), poison-safe `session::lock`, atomic
state writes, strong doc comments, and a large test suite. The `app.rs`
refactor shrank it from ~8,019 to ~3,491 lines.

Open:
- `SqlHighlandView` is still a ~45-field god object and every GUI module is
  `impl SqlHighlandView { ... }`; the type boundary was not split. The
  declined `ConnectionDialogState` / `PendingOps` / `ConnCache` structs
  would address the root cause.
- `complete.rs` (~2,233 lines) and `sql.rs` (~1,721 lines) are large.
- Several long render functions and 3× `#[allow(clippy::too_many_arguments)]`
  in `run.rs`.

## 3. Code smells

- **FIXED:** `ConnectionConfig` debug redaction (see 1.2).
- Inconsistent lock handling: `providers.rs` and parts of `app.rs` use
  `.lock().ok()`/`.map(...)` and silently degrade to empty data on poison,
  while `session::lock` exists to recover. Pick one policy.
- `eprintln!` used for user-facing failures instead of structured logging.
- Formatting is not enforced (e.g. an inline doc comment after
  `pub enum CompleteMode {`); no `rustfmt.toml`/CI `fmt --check`.
- No `#[must_use]` on fallible/query helpers (minor).

## 4. Build / test hygiene

### 4.1 Gui-only tests are not feature-gated — open (bug)
`tests/browser_tree.rs` and `tests/themes.rs` import `sqlhighland::app` /
`sqlhighland::guitheme` (gui-gated) but are not declared with
`required-features = ["gui"]` in `Cargo.toml` (only `ui_picker`/`menus` are).
Confirmed: `cargo clippy --all-targets` / `cargo test` without
`--features gui` fails to compile. Add `[[test]]` entries or a file-level
`#![cfg(feature = "gui")]`.

### 4.2 No CI / toolchain / audit config — open
No GitHub Actions, `rust-toolchain.toml`, `[profile.release]`,
`cargo-audit`, or `deny.toml`. Recommended CI: `cargo fmt --check`,
`clippy -D warnings` (with and without `gui`), `test`, `cargo audit`.

### 4.3 Stale docs — open
`README.md` still says "17 unit tests" (112/116 now), "5 integration tests"
(11 now), "keychain integration is planned" (shipped), and lists CSV export /
schema browser as future work (shipped). `PROGRESS.md` still repeats the
plaintext-password debt.

### 4.4 Package metadata — open
`Cargo.toml` lacks `description`, `license`, and `repository`.

## 5. Rust best practices

- Few production `unwrap`/`expect`, all guarded
  (`app.rs` column fallback, `complete.rs` non-empty group, `main.rs`
  startup). No `unsafe` outside tests.
- `oracledb 26.0.0-beta.3` is a beta dependency (documented, isolated in
  `db.rs`). A transitive `block 0.1.6` (via `gpui-pre` → `cocoa`) reports a
  future-incompat warning upstream; not fixable locally.

## Change set

- `src/fsutil.rs` (new): secure atomic write + `restrict` helper.
- `src/lib.rs`: register `fsutil`.
- `src/config.rs`: secure writes, owner-only migration on load.
- `src/filetab.rs`: crash-safe (fsynced) user-file writes.
- `src/model.rs`: redacting `Debug for ConnectionConfig`.
- Tests: fsutil permissions/round-trip, config migration, model redaction.
