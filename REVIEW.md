# SQLHighland — Code & Security Review

Date: 2026-09-13
Scope: whole tree (`src/`, `tests/`, `Cargo.toml`)
Baseline verified locally: `cargo clippy --features gui --all-targets` clean,
`cargo test --lib` green.

Findings are ordered by severity. Items marked **FIXED** were addressed in the
same change set as this document; the rest are open recommendations.

Update (follow-up change set): the organization findings in §2 and the stale
docs in §4.3 are now fixed too — see the second change set at the end.

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
state writes, strong doc comments, and a large test suite.

**FIXED — file-size / impl sprawl.** The four giants were split by
responsibility using the pattern documented in `ARCHITECTURE.md` (parent file
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
- `SqlHighlandView` remains a ~45-field struct; the `ConnectionDialogState` /
  `PendingOps` / `ConnCache` field-grouping was declined (risk > benefit), so
  the *type* boundary is unchanged even though files are now navigable.
- 3× `#[allow(clippy::too_many_arguments)]` remain in `run/export.rs`.

## 3. Code smells

- **FIXED:** `ConnectionConfig` debug redaction (see 1.2).
- Inconsistent lock handling: `providers.rs` and `app/lsp.rs` use
  `.lock().ok()`/`.map(...)` and silently degrade to empty/stale data on
  poison, while `session::lock` exists to recover. Pick one policy.
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

### 4.3 Stale docs — **FIXED**
`README.md` was refreshed: correct test counts (116 lib / 11 live), the three
password modes, shipped features (autocomplete, schema browser, CSV/XLSX
export), and a pruned roadmap. `PROGRESS.md` remains a historical build log.

### 4.4 Package metadata — open
`Cargo.toml` lacks `description`, `license`, and `repository`.

## 5. Rust best practices

- Few production `unwrap`/`expect`, all guarded
  (`complete/aliases.rs` non-empty group, `app/tabs.rs` pre-checked path,
  `main.rs` startup). No `unsafe` outside tests.
- `oracledb 26.0.0-beta.3` is a beta dependency (documented, isolated in
  `db.rs`). A transitive `block 0.1.6` (via `gpui-pre` → `cocoa`) reports a
  future-incompat warning upstream; not fixable locally.

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
- `ARCHITECTURE.md` (new): module map and the split pattern.
- Docs: `README.md` refreshed.
- Remaining files all < ~1,000 lines; 116 lib tests green, clippy clean.
