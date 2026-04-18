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

LeSwitcheur is signed with a self-signed certificate (no paid Apple Developer ID). Gatekeeper blocks the first launch with *"cannot be opened because the developer cannot be verified"*.

**First launch**: right-click `LeSwitcheur.app` → **Open** → confirm the warning. macOS records the decision, and subsequent launches (including in-place Sparkle updates) run without friction.

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

```sh
./scripts/test-bundle.sh      # or: just test-bundle
```

Wipes saved settings + any stale bundle, rebuilds signed with the local self-signed identity (override via `$CODESIGN_IDENTITY`), prints the resulting signature, then launches the app.

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

## Beyond v0

- Window/app icons in the list
- Auto light/dark theme
- Graphical preferences pane
- Signing and notarization of the `.app`
- Linux / Windows port
