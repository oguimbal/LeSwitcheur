<p align="center">
  <img src="https://leswitcheur.app/screenshots/landing.jpeg" alt="LeSwitcheur panel open" width="820">
</p>

<h1 align="center">LeSwitcheur</h1>

<p align="center">
  <strong>Native macOS task switcher. Switch windows at the speed of thought.</strong>
</p>

<p align="center">
  Fuzzy search across every open window. App launcher, calculator, JavaScript REPL and instant access to AI assistants — all behind a single global hotkey. Written in Rust with <a href="https://www.gpui.rs/">GPUI</a> (<a href="https://zed.dev">Zed</a>'s UI framework). ~8 MB. Zero friction.
</p>

<p align="center">
  <a href="https://leswitcheur.app">leswitcheur.app</a> · <a href="https://leswitcheur.app#download">Download</a>
</p>

---

## Behavior

Headless process. No Dock icon. A global hotkey (default **Opt+Space**) opens a centered panel with a search field and the list of open windows. Fuzzy filtering as you type. Enter activates the window, Escape hides the panel.

Hold **Fn** and two-finger scroll on the trackpad to walk back through your recent window history without touching the keyboard.

## Prerequisites

- macOS 13+
- **Full Xcode** (not just the Command Line Tools) — GPUI compiles its Metal shaders at build time, which requires the `metal` tool shipped only with `Xcode.app`.
  - Install from the App Store, then: `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`
- Stable Rust (via [rustup](https://rustup.rs))
- Optional: [`just`](https://github.com/casey/just) (`brew install just`)

## Installing a release build

Public builds are signed with a Developer ID certificate, notarised by Apple and stapled, so Gatekeeper accepts them on first launch — no right-click → Open dance required.

## Required permission

On first launch, macOS prompts for **Accessibility** permission (needed to list and activate windows of other apps). Grant it in System Settings → Privacy & Security → Accessibility.

## Build & run

The binary (`switcheur`) requires full Xcode (see above) because GPUI compiles its Metal shaders during the build. Without Xcode, only the library crates (`switcheur-core`, `switcheur-platform`, `switcheur-ui`) compile — useful for iterating on logic and unit tests.

```sh
# Check and test the pure-Rust part (no Xcode required):
cargo check -p switcheur-core -p switcheur-platform -p switcheur-ui
cargo test -p switcheur-core

# Binary + app (requires Xcode):
cargo run -p switcheur
cargo build --release -p switcheur
./bundle/bundle.sh                # produces dist/LeSwitcheur.app
```

With `just`:

```sh
just check
just test
just run
just dev      # watch + rebuild + restart with --open (requires cargo-watch)
just bundle
```

### CLI flags

- `--open`: immediately open the panel at startup (as if the hotkey had been pressed). Handy in dev.

### Test a production build from scratch

macOS:

```sh
./scripts/test-bundle.sh      # or: just test-bundle
```

Wipes saved settings + any stale bundle, rebuilds signed with the local self-signed identity (override via `$CODESIGN_IDENTITY`), prints the resulting signature, then launches the app.

Windows:

```powershell
.\scripts\test-build.ps1              # build + launch (config preserved)
.\scripts\test-build.ps1 -Reset       # wipe config + cache + logs first
.\scripts\test-build.ps1 -Attach      # launch + tail log file
.\scripts\test-build.ps1 -Help        # full help
```

`-Attach` tails `%LOCALAPPDATA%\fr.gmbl.LeSwitcheur\logs\switcheur.log` because the release exe runs with `windows_subsystem = "windows"` (no console). For Win+R one-liners:

```
powershell -NoExit -File "C:\Users\Home\repos-my\LeSwitcheur\scripts\test-build.ps1" -Attach
```

## Structure

```
crates/
  switcheur/           # bin + wiring
  switcheur-core/      # domain, fuzzy, config, state (pure Rust)
  switcheur-platform/  # macOS APIs (CGWindowList, AX, hotkey)
  switcheur-ui/        # GPUI views, actions, theming
bundle/
  Info.plist           # LSUIElement=true, NSAccessibilityUsageDescription
  bundle.sh            # assembles the .app
```

## Configuration

TOML file loaded from `~/Library/Application Support/LeSwitcheur/config.toml`. Created with defaults on first launch.

## Releasing

```sh
just release          # patch bump (0.1.9 -> 0.1.10)
just release minor    # 0.1.9 -> 0.2.0
just release major    # 0.1.9 -> 1.0.0
just release 0.3.1    # explicit version
just release patch && gh run watch # watch result
```

Bumps the version in `Cargo.toml` + `bundle/Info.plist`, commits, tags `vX.Y.Z`, pushes. The tag push triggers `.github/workflows/release.yml`, which builds, signs with the **Developer ID Application** identity (hardened runtime + secure timestamp + `bundle/entitlements.plist`), submits the `.app` and the `.dmg` to Apple's notary service, staples both, and uploads the stapled `.dmg` to a GitHub Release. End users get a Gatekeeper-clean DMG with no right-click → Open dance.

Pass `--no-push` to commit + tag locally without pushing (handy when reviewing the bump).

Required repository secrets (one-time setup): `CODESIGN_CERT_P12_BASE64`, `CODESIGN_CERT_PASSWORD`, `CODESIGN_IDENTITY`, `NOTARY_APPLE_ID`, `NOTARY_TEAM_ID`, `NOTARY_PASSWORD`. See `AGENTS.md` for the values.

## Beyond v0

- Window/app icons in the list
- Auto light/dark theme
- Graphical preferences pane
- Linux / Windows port
