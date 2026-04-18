# AGENTS.md — Guide for Claude and other agents

What agent (or human) need to work here efficiently. Keep current when architecture change.

## TL;DR

LeSwitcheur = macOS task switcher. Rust + **GPUI** (Zed UI framework). Headless process, global hotkey open centered panel with input + fuzzy-filtered list of open windows. Enter activate, Escape close.

Status: v0 scaffolding done. Pure logic (`switcheur-core`) fully tested. macOS platform (`switcheur-platform`) + GPUI views (`switcheur-ui`) written + compile. Binary + bundling done.

## Crate map

| Crate | Role | Pure Rust? | Tested? |
|-------|------|------------|---------|
| `crates/switcheur-core` | Domain (WindowRef, AppRef), fuzzy matcher (nucleo), TOML config, UI state machine | yes | 13 unit tests |
| `crates/switcheur-platform` | `WindowSource` trait + macOS impl (CGWindowList, AXUIElement, NSWorkspace, global-hotkey) | no (cfg macOS) | not yet |
| `crates/switcheur-ui` | GPUI views: `SwitcherView`, query input, list, theme, action key bindings | no (GPUI) | not yet (GPUI UI testing ecosystem young) |
| `crates/switcheur` | Binary: boot, wire hotkey → platform → view | no | n/a |

## Validated product decisions

**Do not** relitigate unless user ask:

- **Hotkey**: configurable, default **Ctrl+=** (Cmd+Space clash Spotlight; Opt+Space clash some system shortcuts).
- **Scope**: windows only by default. `include_apps: bool` in config also list running apps.
- **Release artifact**: `.app` bundle via `bundle/bundle.sh` + `bundle/Info.plist`.
- **GPUI**: git dep on `zed-industries/zed`, **pinned SHA** for reproducibility (manual bump).
- **Structure**: Cargo workspace, multiple crates. No single-crate design.

## Commands

```sh
# Pure logic, no Xcode or Metal needed:
cargo check -p switcheur-core -p switcheur-platform -p switcheur-ui
cargo test -p switcheur-core

# Binary (needs full Xcode — see "Build prerequisites" below):
cargo run -p switcheur
cargo run -p switcheur -- --open   # auto-open the panel at startup
cargo build --release -p switcheur

# .app bundle:
./bundle/bundle.sh         # produces dist/LeSwitcheur.app

# With just:
just check / just test / just run / just dev / just bundle
```

`just dev` run `cargo watch -c -w crates -x 'run -p switcheur -- --open'`. Need `cargo install cargo-watch`.

## Build prerequisites

- **Stable Rust** (via rustup). `rust-toolchain.toml` at repo root pin channel.
- **Full Xcode** (not just CLT) for binary. GPUI compile Metal shaders in build script, call `xcrun metal` — only ship with Xcode.app.
  - Check: `xcrun --find metal` succeed.
  - Setup: install Xcode, then `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`, then `sudo xcodebuild -license`, then `sudo xcodebuild -runFirstLaunch`.
  - **`runtime_shaders`** feature on `gpui_platform` **enabled** in workspace: stitch shaders at runtime instead of precompile. Avoid install separate "Metal Toolchain" component (download via `xcodebuild -downloadComponent MetalToolchain` — Apple CDN often fail). Without flag, build script fail with "cannot execute tool 'metal' due to missing Metal Toolchain".
  - **`font-kit`** feature also enabled; without it, system fonts no load + all text render invisible.
  - No Xcode: only library crates compile; binary fail at `cargo check -p switcheur`.

## Key external dependencies

| Dep | Version | Role |
|-----|---------|------|
| `gpui` | pinned git rev (zed-industries/zed) | UI framework |
| `gpui_platform` | same git rev, features `runtime_shaders` + `font-kit` | Provides `application()` helper that instantiates the platform |
| `nucleo-matcher` | 0.3 | Fuzzy matching (same engine as Zed) |
| `global-hotkey` | 0.6 | Carbon `RegisterEventHotKey` wrapper |
| `objc2` / `objc2-app-kit` / `objc2-foundation` | 0.6 / 0.3 / 0.3 | NSRunningApplication, NSWorkspace |
| `core-foundation` / `core-graphics` | 0.10 / 0.25 | CGWindowList |
| `accessibility-sys` | 0.1 | AXUIElement raise + permissions |

### Bumping GPUI

```sh
git ls-remote https://github.com/zed-industries/zed refs/heads/main
```

Replace SHA in `Cargo.toml` (two entries: `gpui` + `gpui_platform`). Run `cargo update`. Expect breaking API changes — GPUI move fast between commits.

## GPUI-specific gotchas

- **No `set_visible`/`hide`/`show`** on `Window` in this SHA. To "close" switcher: `window.remove_window()`. To "reopen": fresh `cx.open_window(options, builder)`.
- **No `Application::new()`** — only `Application::with_platform(...)`. Use `gpui_platform::application()` which handle boilerplate.
- **`cx.open_window(options, |window, cx| -> Entity<V>)`** builder return `Entity<V>`, not `Context<V>`. Use `cx.new(|cx| view_struct)` inside.
- **Cross-entity subscription**: to listen view events from bin, capture `Entity<V>` during builder into `Rc<RefCell<Option<Entity<V>>>>`, then `AsyncApp::subscribe(&entity, |e, ev, app| ...)`.
- **`AsyncApp::update(fn)` return `R`** (not `Result<R>`). No `?`. `Result`-returning variant is `AsyncWindowContext::update`.
- **Accessibility permission**: `CGWindowList` work without it, but `AXUIElementPerformAction(kAXRaiseAction)` not. Call `ensure_accessibility(prompt=true)` at boot to trigger system dialog.
- **Focus**: `.track_focus(&handle)` only track; grab focus need `handle.focus(window, cx)` explicitly at open time, after `cx.new`.

## macOS-specific gotchas

- `NSRunningApplication::activateWithOptions` deprecated on macOS 14+ but still work. `activate()` (no options) not yet exposed by `objc2-app-kit 0.3`. Local `#[allow(deprecated)]` fine.
- `kCGWindowListOptionOnScreenOnly` + friends live in `core_graphics::window` (not `::display`) since core-graphics 0.25.
- Bundle `Info.plist`: **LSUIElement = true** (no Dock icon) + **NSAccessibilityUsageDescription** required, else system prompt never appear.
- **Cross-Space / cross-fullscreen window activation** (`macos/activate.rs`): three layers, ALL required.
  1. `NSRunningApplication.activateFromApplication:options:` — yield-based. Only path crossing Dock "universal owner" gate + macOS 14+ "caller must hold activation" rule. Source must be active.
  2. SLPS: `_SLPSSetFrontProcessWithOptions` + two `SLPSPostEventRecordTo` with `[0x20..0x30]=0xff*16` (DO NOT omit) — pick specific `CGWindowID`.
  3. `AXUIElementPerformAction(kAXRaiseAction)` — same-Space z-order. Skip if AX not surface window.
  Don't write `kAXMain` / `kAXFocused` post-raise — race SLPS, break keyboard focus. Use `WindowRef.id` (captured at enum time); don't re-derive via AX (often hide cross-Space windows at activation time).
- **LSUIElement + `cx.activate(true)`**: accessory app not "active" from window focus alone. Need explicit `cx.activate(true)`:
  1. `WindowKind::Normal` / `Floating` (settings, onboarding) — else no foreground.
  2. `WindowKind::PopUp` that trigger target-app activation on confirm (the switcher) — else yield-based activation above return `NO` silently, cross-Space switch break.
- **Switcher confirm order**: activate target BEFORE close panel (`handle_view_event`). Close first strip activation → yield no-op. Yield API transfer keyboard focus atomically so late close safe.
- Karabiner Elements / non-US keyboard layouts: `global-hotkey` map to physical W3C key codes. `Code::Equal` bind to US `=` physical position, sit under different keycaps on AZERTY/QWERTZ. Prefer keys unambiguous across layouts (letters, digits, Space, arrows) for default.

## User configuration

- Path: `~/Library/Application Support/fr.gmbl.LeSwitcheur/config.toml` (via `directories::ProjectDirs("fr", "gmbl", "LeSwitcheur")`).
- Fields: `hotkey` (`{ modifiers: [...], key: "..." }`), `include_apps: bool`, `appearance: "system" | "light" | "dark"`.
- First-run: if file missing, `Config::default()` written to disk.
- `deny_unknown_fields`: catch typos. If add fields with backwards compat concerns, drop this or use `serde(alias)`.

## Key files

- `Cargo.toml` (root) — workspace members, shared versions, GPUI SHA.
- `crates/switcheur-core/src/matcher.rs` — nucleo wrapper. Output: `MatchResult { item, score, indices }`.
- `crates/switcheur-core/src/state.rs` — `SwitcherState`: input → reranking → selection.
- `crates/switcheur-platform/src/macos/windows.rs` — on-screen window enumeration.
- `crates/switcheur-platform/src/macos/activate.rs` — AX raise for specific window (not just frontmost).
- `crates/switcheur-platform/src/macos/hotkey.rs` — `HotkeySpec` → global-hotkey `HotKey` + `HotkeyEvent` channel.
- `crates/switcheur-ui/src/switcher_view.rs` — root GPUI view, actions, `on_key_down`.
- `crates/switcheur/src/main.rs` — boot + async hotkey loop.
- `bundle/bundle.sh` + `bundle/Info.plist` — `.app` assembly.

## Not in v0 (roadmap)

- Window / app icons in list (`NSWorkspace::iconForFile`, caching).
- Graphical preferences pane.
- Signing + notarization of `.app`.
- Auto light/dark theme via `effectiveAppearance`.
- Linux / Windows port — `WindowSource` trait already shaped for it, only need new impl.
- GPUI UI tests — ecosystem young, revisit later.

## Local conventions

- Comments: only when "why" non-obvious. No doc that paraphrase code.
- Errors: `anyhow::Result` for user-facing errors, `thiserror` when caller need to match on them.
- **English only** in the repo: all docs, code, comments, commit messages, and config. No French anywhere except:
  - i18n translation files under `crates/switcheur-i18n/locales/` (FR source of truth: `fr.yml`).
  - Test fixtures that explicitly assert a translated string or exercise non-ASCII UTF-8 (e.g. `"Réglages"` in `crates/switcheur-i18n/src/lib.rs`, `"é"` in `crates/switcheur-ui/src/input.rs`).
- User-facing chat with the maintainer stays in whatever language they write in (often French) — but none of that language lands in the repo.