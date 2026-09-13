# Building the macOS app (cargo-packager)

How SQLHighland is bundled into a native `.app` and `.dmg` using
[CrabNebula's `cargo-packager`](https://github.com/crabnebula-dev/cargo-packager).

Status: produces a working **unsigned** arm64 `.app` + `.dmg` today. Code
signing / notarization is not wired up yet (§5).

---

## 1. Prerequisites

- Full Xcode + the Metal toolchain (see the README "Prerequisites" section) —
  GPUI renders via Metal.
- Rust ≥ 1.89.
- The packager CLI (not a repo dependency):

  ```sh
  cargo install cargo-packager --locked
  ```

## 2. Build

```sh
cargo packager --release
```

Outputs, under `dist/` (git-ignored):

- `dist/SQLHighland.app`
- `dist/SQLHighland_<version>_<arch>.dmg` (e.g. `SQLHighland_0.1.0_aarch64.dmg`)

The packager does not compile on its own: it first runs the configured
`before-packaging-command`, which **must** include `--features gui` because the
`sqlhighland` binary is feature-gated.

## 3. Configuration

Lives in `Cargo.toml` (read from `package.metadata.packager`):

```toml
[package.metadata.packager]
product-name = "SQLHighland"
identifier = "com.srikkanthm.sqlhighland"
category = "DeveloperTool"                 # -> LSApplicationCategoryType
before-packaging-command = "cargo build --release --features gui"
formats = ["app", "dmg"]
out-dir = "dist"
icons = ["assets/icon/icon.icns"]

[package.metadata.packager.macos]
minimum-system-version = "11.0"
```

`binaries` is intentionally omitted: the packager derives it from
`cargo metadata`. A full field reference is in the
[`cargo-packager` config docs](https://docs.rs/cargo-packager/latest/cargo_packager/config/struct.Config.html);
the repo also accepts a standalone `Packager.toml`/`packager.json`.

## 4. Icon

Source of truth is `assets/icon/icon.svg`, drawn on Apple's macOS icon grid:
a 1024×1024 canvas with the artwork in a centered **824×824 continuous-corner
rounded square** (corner radius 185.4, Figma-style corner smoothing 0.7) and a
100px transparent margin on every side. The scene is clipped to that shape.
The generated `icon-1024.png` and `icon.icns` are checked in so packaging needs
no extra tooling; regenerate them only when the SVG changes:

```sh
brew install librsvg
cd assets/icon
rsvg-convert -w 1024 -h 1024 icon.svg -o icon-1024.png
mkdir icon.iconset
for s in 16 32 128 256 512; do
  sips -z $s $s icon-1024.png --out "icon.iconset/icon_${s}x${s}.png"
  sips -z $((s*2)) $((s*2)) icon-1024.png --out "icon.iconset/icon_${s}x${s}@2x.png"
done
iconutil -c icns icon.iconset -o icon.icns
rm -rf icon.iconset
```

The packager writes the icon into `SQLHighland.app/Contents/Resources/icon.icns`
and sets `CFBundleIconFile`.

## 5. Signing & notarization (not configured)

The produced app is **unsigned**. On another Mac, Gatekeeper will refuse the
first launch: right-click → Open, or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/SQLHighland.app
```

To ship properly signed outside your machine you need an Apple Developer ID:

- `macos.signing-identity = "Developer ID Application: NAME (TEAMID)"`
- `macos.entitlements = "path/to/entitlements.plist"` (hardened runtime;
  network + login-keychain access work without extra entitlements in a
  non-sandboxed app)
- notarization via `macos.notarization-credentials` (Apple ID + app-specific
  password, or an API key)

The certificate bytes / password (`signing-certificate`,
`signing-certificate-password`) can only be passed through the CLI, never the
config file. None of this is set up yet.

## 6. Architecture: Apple Silicon only

SQLHighland targets **Apple Silicon (arm64 / `aarch64-apple-darwin`) only**.
There is no Intel (`x86_64-apple-darwin`) or universal build, by design. The
packager's default host target (arm64 on Apple Silicon) is therefore exactly
what we want — no `target-triple` override is needed. Do not add
`lipo`/universal steps, and treat builds from an Intel Mac as unsupported.

## 7. Verify a build

```sh
# launch the bundle (window should appear; quit normally)
open dist/SQLHighland.app

# inspect the disk image, then detach
hdiutil attach -nobrowse -readonly dist/SQLHighland_0.1.0_aarch64.dmg
hdiutil detach /Volumes/SQLHighland
```

The DMG contains `SQLHighland.app` plus an `Applications` symlink for
drag-to-install. App data still lives in `~/.config/sqlhighland/`
(`connections.toml`, `preferences.toml`, `tabs/`).
