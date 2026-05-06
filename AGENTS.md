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
- **Release artifact**: `.app` bundle via `bundle/bundle.sh` + `bundle/Info.plist`. Public builds: signed Developer ID + notarised + stapled, packaged as `.dmg`.
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
| `coreaudio-sys` | 0.2 | `kAudioHardwarePropertyProcessObjectList` for currently-producing PIDs |
| `libproc` | 0.14 | `pbi_ppid` walk: browser-helper PID → main app PID |
| MediaRemoteAdapter (vendored) | upstream `ungive/mediaremote-adapter` BSD-3 | Bypass for macOS 15.4+ now-playing daemon check; spawned via `/usr/bin/perl` |

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
- **Window activation** (`macos/activate.rs`): match AltTab `Window.focus()` and yabai. Two paths.
  - **Path A (default)** — got AX element + `CGWindowID`. SLPS sequence then `AXRaise`.
    1. SLPS: `_SLPSSetFrontProcessWithOptions(psn, wid, kCPSUserGenerated=0x200)` + two `SLPSPostEventRecordTo` with `bytes[0x20..0x30]=0xff*16` (DO NOT omit). Same byte layout as AltTab + yabai. Triggers Space switch when needed AND restores from Dock if minimized — `SLPS = Dock-icon-click semantics`.
    2. `AXUIElementPerformAction(kAXRaiseAction)` — z-order nudge.
  - **Path B (fallback)** — no AX element AND brute-force lookup also failed. Whole-app activate via `NSApp.deactivate()` then `running.activateWithOptions(.ActivateAllWindows)` (deprecated API still works on 15). Last-resort, lifts every window of app above other apps' so don't enter for normal pick — only when SLPS un-targetable.
  - **Brute-force AX** — when `kAXWindowsAttribute` returns empty for owning app (cross-Space "AX hierarchy suspended", e.g. Chess on another Space), iterate private AXUIElementID 0..1000 via `_AXUIElementCreateWithRemoteToken(token)` (token = `pid` LE i32 + 4-byte zero + 0x636f636f magic + AXUIElementID LE u64). Match by `_AXUIElementGetWindow == target_wid`. Caller `CFRelease`s. Cap iteration at ~100 ms wall-clock. Once recovered, the AX element drops back into Path A.
  - **Don't pre-fire `kAXMinimizedAttribute=false`** ahead of SLPS — that starts the genie animation on the window's origin Space (wrong Space when cross-Space) and the subsequent SLPS/AXRaise lands during the animation and gets dropped. Let SLPS do the un-minimize itself.
  - **Don't use `activateFromApplication:options:`** — modern yield API. Returns `ok=true` for cross-Space-with-suspended-AX but produces no visible effect from an LSUIElement caller. SLPS without it works, AltTab + yabai don't use it.
  - **Don't write `kAXMain` / `kAXFocused` post-SLPS** — races key-window transition, leaves keyboard focus on previous app. AltTab + yabai omit them.
  - **Use `WindowRef.id` captured at enum time** — re-deriving via AX often fails for cross-Space windows.
- **LSUIElement + `cx.activate(true)`**: accessory app not "active" from window focus alone. Need explicit `cx.activate(true)`:
  1. `WindowKind::Normal` / `Floating` (settings, onboarding) — else no foreground.
  2. `WindowKind::PopUp` (the switcher) — else keyboard input not delivered.
- **Switcher confirm order**: panel still active during `activate_window`, close after. Path A SLPS works regardless of caller activation state. Closing before risks dropping the click before SLPS posts.
- Karabiner Elements / non-US keyboard layouts: `global-hotkey` map to physical W3C key codes. `Code::Equal` bind to US `=` physical position, sit under different keycaps on AZERTY/QWERTZ. Prefer keys unambiguous across layouts (letters, digits, Space, arrows) for default.

## Regression scenarios — focus + listing

Window listing + activation are flaky-prone. macOS APIs (AX, CG, SkyLight private, NSRunningApplication) interact in undocumented ways and behave differently across versions, app types, and Space states. **Before any commit/release that touch `activate.rs`, `windows.rs`, or `main.rs` Confirmed-event path, walk this list manually.** Don't trust "build" or "new case work" — past fix repeatedly broke adjacent case.

If can't test a case: say so explicit, don't claim still work.

### Activation cases

1. **Alt-tab back order preservation** — pick window of app A from app B, immediately Cmd+Tab again must return to B. `.ActivateAllWindows` lift every window of A above B → break this.
2. **Same-Space normal window focus** — no Space switch, target front, keyboard follow.
3. **Cross-Space focus (non-fullscreen)** — OS switch Space. Test both cross-app + cross-Space.
4. **Cross-Space focus (target on fullscreen Space)** — entering fullscreen.
5. **Cross-Space focus (we're inside fullscreen Space)** — exiting fullscreen.
6. **Un-minimize from Dock** — AX un-minimize, no concurrent SLPS (Chrome restore animation glitch).
7. **AX-suspended cross-Space app** (e.g. Chess on another Space) — AX hierarchy return 0 windows even though CG see them. Must still focus + Space-switch. Path A (SLPS+AXRaise) silently no-op here; Path B fallback needed.

### Listing cases

8. **Eclipse RCP "PartRenderingEngine's limbo" windows** at `(-10000, -10000)` — filter via geometry-vs-display.
9. **Keychain Access / System Settings / Mail "ghost" windows** post red-X — filter via SkyLight Spaces empty / "lives only on active Space when off-screen".
10. **Hidden/scratch surfaces** (zero bounds, zero alpha, non-layer-0) — filter via CG layer/alpha/bounds.
11. **Finder hidden desktop window** — reject via `ALLOWED_SUBROLES`.
12. **Dock / SystemUIServer / WindowServer** — never in list. `activationPolicy() == Regular` filter on `list_apps`.
13. **`show_all_spaces` toggle** — off → current Space only; on → all Spaces. Minimized always kept (current-Space conceptually).
14. **Untitled CG-only entries** — at most 1/pid, only when no titled coverage from AX or CG.

### Browser

15. **Browser tab focus** (Safari/Chrome/Arc) — pick tab row → focus right window AND right tab.
16. **Browser window vs tab** — typing window title hit `Window`, tab title hit `BrowserTab`.
17. **Tab list refresh async** — UI spinner without blocking switcher.

### Panel dismiss

18. **Foreign-app hotkey while panel open** — open switcher, press global hotkey owned by another app (e.g. Warp Guake-mode F1). Panel must dismiss within ~150ms. Two paths cover this: workspace activation (when the other app gains app-level focus) and CG z-order polling (when the other app shows a non-activating panel that doesn't fire workspace notif). GPUI window-active flip alone unreliable for `WindowKind::PopUp`.
19. **Programmatic activation by another process** — open switcher, run `osascript -e 'tell application "Safari" to activate'`. Panel must dismiss.
20. **Dock click while panel open** — click another app in Dock → panel dismisses.
21. **Same-process panel activation (Settings / Onboarding)** — open switcher, open Settings. Panel must dismiss. Drives **existing GPUI** observer, not workspace path (workspace path filter self-pid).
22. **License activation flow exempt** — `NagPhase::Activating`: open browser to confirm license must NOT dismiss panel. Outstanding poll keeps running.
23. **Open With popover exempt** — click inside owned non-activating popover must NOT dismiss panel.
24. **Cmd+Tab grace period** — silent in-memory cycle (no panel yet, `state.current` is `None`) → loop is no-op, no panel flash.

### Currently Playing

25. **MediaRemote helper present** — bundled `.app` carries `Contents/Frameworks/MediaRemoteAdapter.framework` + `Contents/Resources/mediaremote/mediaremote-adapter.pl`. Both must be code-signed by the same identity as the app. Verify via `codesign --display --verbose=2 dist/LeSwitcheur.app/Contents/Frameworks/MediaRemoteAdapter.framework`.
26. **Playing browser tab disambiguation** — Chrome with two YouTube tabs, only background one playing → row pins the audible tab via the helper's title metadata; Enter focuses that tab. Without the helper (un-bundled / 15.4+ direct dlopen path) the row falls through to "Chrome · Now Playing" with no tab — never the wrong tab.
27. **Paused source via MediaRemote fallback** — Spotify paused, AppleScript automation prompt denied (TCC) → MediaRemote helper still surfaces the row with the current track + paused badge. Browsers excluded from this fallback (would double-list with the CoreAudio browser row).
28. **Helper invocation cost** — perl + framework load ≈ 200–400 ms cold. The probe runs off the UI thread on switcher open; first-paint shows the rest of the panel immediately, audio rows appear shortly after.
29. **Daemon refusal silent** — `current_now_playing` returns `None` on every error path (no helper, helper crashed, bad JSON, daemon refused even via perl). No log spam.
30. **`LESWITCHEUR_BUNDLE` env override** — `cargo run --example probe_audio` resolves the helper via this var pointed at `dist/LeSwitcheur.app`, since `NSBundle::mainBundle()` from a cargo example points at the example binary's parent dir.

### Don't-do

- AppleScript for window mgmt — banned.
- Direct `dlopen` of MediaRemote.framework from our binary — silently rejected on macOS 15.4+ (daemon-side bundle-id check). Always go through the bundled perl helper.
- `kAXMain` / `kAXFocused` write after SLPS — race, key focus stuck on previous app.
- Cross-Space target with AX-empty: don't fall through to "first window" — that's a sibling on *current* Space, SLPS-targeting it silently no-op.
- `.ActivateAllWindows` for the N-of-M same-Space case — break #1.
- Workspace dismiss filter on `bundle_id` alone — self-pid is reliable; bundle id defence in depth only.
- Per-panel subscribe/unsubscribe to `subscribe_app_activations` — observer always-on, loop short-circuits when `state.current` is `None`.
- Panel-watch poll without min-bounds filter — tooltips and HUD scratch surfaces flash on/off and would dismiss the panel mid-use. Keep `w >= 200 && h >= 100` in `onscreen_app_window_ids_excluding_pid`.
- Panel-watch dismiss on first tick — snapshot lazily on the first tick after open, only dismiss when delta appears on a *subsequent* tick, otherwise we'd dismiss on the panel's own appearance noise.

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
- `crates/switcheur-platform/src/macos/now_playing.rs` — spawn `/usr/bin/perl` + bundled MediaRemoteAdapter.framework; parse JSON. Bypass for macOS 15.4+ daemon-side bundle-id check.
- `crates/switcheur-platform/src/macos/audio.rs` + `media_apps.rs` — CoreAudio enumeration (currently producing) + AppleScript player-state probes (Spotify / Music). MediaRemote helper fills paused-source rows when these miss.
- `bundle/mediaremote/` — vendored upstream sources (`ungive/mediaremote-adapter`, BSD-3) + `build.sh` (clang flat invocation, universal arm64+x86_64).
- `bundle/bundle.sh` + `bundle/Info.plist` — `.app` assembly.

## Releasing

When maintainer say "release", run `just release` (or `./scripts/release.sh`). Default bump = patch; `just release minor` / `just release major` / `just release 0.3.1` override. `--no-push` flag for dry run.

`scripts/release.sh` only bump + commit + tag + push. **CI does the heavy lifting** — build, sign, notarise, publish to GitHub Releases, all driven by the tag push (`.github/workflows/release.yml`). Local laptop never produces a public artifact, so the release is reproducible from the tag commit.

Local script step:

1. Refuse if working tree dirty or off master.
2. Bump `Cargo.toml` workspace version + `bundle/Info.plist` `CFBundleShortVersionString`.
3. `cargo update --workspace` to refresh `Cargo.lock`.
4. `bundle/verify-version.sh` to assert Cargo.toml ↔ Info.plist lockstep.
5. `git commit -m "release vX.Y.Z"` + `git tag -a vX.Y.Z`.
6. `git push origin master && git push origin vX.Y.Z` (skipped with `--no-push`).

CI step (on tag push):

1. Import Developer ID p12 into temp keychain (from `CODESIGN_CERT_P12_BASE64` + `CODESIGN_CERT_PASSWORD` secrets).
2. `bundle/bundle.sh` — sign `.app` with hardened runtime + timestamp + `bundle/entitlements.plist`.
3. `bundle/notarize.sh dist/LeSwitcheur.app` — zip, submit to Apple, staple.
4. `bundle/dmg.sh` — build + sign DMG.
5. `bundle/notarize.sh dist/LeSwitcheur.dmg` — submit + staple.
6. `softprops/action-gh-release@v2` upload stapled DMG to GitHub Release.

### Required GitHub repository secrets

| Secret | Value |
|---|---|
| `CODESIGN_CERT_P12_BASE64` | base64 of Developer ID Application `.p12` (cert **+** private key). Export from Keychain Access → right-click identity → Export → format `.p12`. Then `base64 -i cert.p12 \| pbcopy`. |
| `CODESIGN_CERT_PASSWORD` | Password set during `.p12` export. |
| `CODESIGN_IDENTITY` | `Developer ID Application: Olivier Guimbal (Q966PUVAXJ)` |
| `NOTARY_APPLE_ID` | `olivier.guimbal@gmail.com` |
| `NOTARY_TEAM_ID` | `Q966PUVAXJ` |
| `NOTARY_PASSWORD` | App-specific password from appleid.apple.com (Sign-In and Security → App-Specific Passwords). |

If any signing/notarisation secret is missing, CI either falls back to ad-hoc (signing) or fails outright (notarisation). The fail-out is intentional — a public build without notarisation would mislead users.

### Local notarisation (rare)

`bundle/notarize.sh` also work locally via `xcrun notarytool` keychain profile `leswitcheur-notary` (override via `$NOTARYTOOL_PROFILE`). Used for one-off manual builds; not part of `just release`.

### Update manifest

`crates/switcheur/src/main.rs:blocking_update_check` poll `https://leswitcheur.app/api/updates/latest` for `{ version, url, ... }`. After CI publish, server-side manifest must be bumped to advertise new GitHub Release `.dmg` URL — otherwise running install never see update. Not yet automated.

## Not in v0 (roadmap)

- Window / app icons in list (`NSWorkspace::iconForFile`, caching).
- Graphical preferences pane.
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