# Desktop app

A macOS menu-bar companion for `lets-triage-mail`. It is a second front end onto the **same**
engine the CLI runs: it never re-implements triage, never reads the config file or the state
database, and never speaks IMAP or calls the model. It owns only the engine **process** and the
operating-system integration (tray icon, one window, quit behaviour).

The CLI stays the supported way to run the engine. The desktop is post-v1, in development, and keeps
every hard rule from `CLAUDE.md`.

## Repository boundaries

```
Cargo.toml            # the Rust crate (root). `exclude = ["desktop/"]` keeps the app out of it.
src/**                # the engine and CLI. Does not know the desktop exists.
desktop/              # the Electron app: its own package.json, no Cargo involvement.
  src/                # main process + controllers (TypeScript, ESM)
  renderer/           # two pages, no framework, no remote resources: the window and the popup
  assets/             # the menu-bar template icon (18 px + 36 px @2x)
  test/               # node --test unit tests + synthetic fixtures
docs/DESKTOP.md       # this file
```

The engine is a Rust crate; the desktop is not. There is no napi/WASM embedding, no TCP port and no
shared database access. The only seam is the engine's **command line**: the desktop spawns
`lets-triage-mail` as a child process and renders what the engine reports.

### Who owns what

| Engine (Rust) owns | Desktop (TypeScript) owns |
|---|---|
| Classification, flag mapping, digest, IMAP, SQLite state | Process lifecycle of the engine (start, stop, show state) |
| Config parsing **and validation** (`Config::load`/`validate`), password lookup (`password_command`) | Presentation of what the engine reports; OS integration (tray, dock, window, later notifications) |
| Every hard rule in `CLAUDE.md` (never red, own flags only, no bodies, local model only) | Nothing that touches mail, the model, the DB or the config file's contents |

The desktop never parses TOML, validates a setting, opens the database, speaks IMAP or calls the
model. If it needs a fact it asks the engine and renders the answer verbatim. This is enforced by
`desktop/test/boundaries.test.ts`, the same way `imap/` is checked in Rust.

## Interface contract

All calls are engine commands. `check`, `run` and `--version` already existed; **`status --json` is
new**, added for the popup, and it is the only command the engine changed for the desktop's sake.

| Call | Use | Shape the desktop relies on |
|---|---|---|
| `lets-triage-mail run` (long-lived child) | the triage engine | stdout/stderr are free-form logs: kept in a ring buffer (last 500 lines, memory only), **never** parsed. Exit 0 = clean stop; non-zero = failed (config missing, or every account failed). |
| `lets-triage-mail check --json` (one-shot) | "Check setup": the engine validates config, model and accounts; the window lists the `items` | `{ok, failed, warnings, items:[{area, status:"ok"|"warn"|"fail", message, hint: string|null}]}`. Exit is non-zero when something failed but **stdout is still the JSON**: parse stdout, ignore the exit code. A config that does not load is itself a `Config` fail item, so validation is the engine's. Shape is locked by the existing test in `src/check.rs` (~line 1404). |
| `lets-triage-mail status --json` (one-shot) | the popup under the tray icon: engine liveness, today's counts, every account's state | `{version:1, generated_at, engine:{running, started_at, heartbeat_at}, window:{kind:"today", since}, counts:{to_look_at, fyi, done}, accounts:[{name, mode, state, reason, counts}]}`. Timestamps are Unix seconds and `null` when not running (a key is never missing). `state` is one of `connecting | connected | reconnecting | sign_in_failed | failed | not_running`; `reason` is the engine's own fixed English sentence. **Parse stdout; a non-zero exit with empty stdout is an error** (the first stderr line is the reason); a read is killed after 10 s. Metadata only — never a sender, subject, body or raw server error text. Shape is locked by `status::tests::json_shape_is_locked` in `src/status.rs`. |
| `lets-triage-mail --version` | show the engine version in the tray menu | one line `lets-triage-mail X.Y.Z` |

Config and database are the **same files the CLI uses**: the desktop passes no `--config` unless
`LTM_CONFIG` is set. One setup, two front ends.

Changing a `check --json` or `status --json` field is a breaking change for the desktop; the Rust
lock tests are the guard. `status --json` also carries `version: 1` and the parser refuses a shape
version it does not speak, rather than guessing at a renamed key.

## Lifecycle contract

Engine states: `stopped`, `starting`, `running`, `stopping`, `failed(exit code or signal, last
stderr lines)`. **`running` in the tray means the child process is alive**; health and per-account
progress come from `status` (`check` still answers "is my setup valid"). The popup's `running` is a
different fact from the same word: it is the engine's **heartbeat** row being fresh (`status --json`
says `running: false` within 90 s of a crash, and at once on a clean stop, because `run` deletes the
row). A hard-killed engine child therefore still shows a stale tray state for up to 90 s in the popup
while the tray already says `failed`.

- `start()`: resolve the binary, spawn `[bin, "run"]` with stdio `["ignore","pipe","pipe"]`.
  `starting → running` on the `spawn` event.
- Unexpected exit while `running` → `failed`; the tray shows it. **No automatic restart** in this
  increment (a process-level restart policy with backoff is a later increment; the engine already
  restarts its own account workers).
- `stop()`: send **SIGTERM once**, wait up to **30 s**, then SIGKILL. 30 s matches `ExitTimeOut` in
  `contrib/launchd/*.plist` (grace 10 s plus margin). **Never a second SIGTERM or any SIGINT**:
  `src/cli/run.rs::watch_for_signals` treats a second signal as "exit now with 130, skip cleanup".
- **Closing the window (red button, Cmd+W) only hides it.** Tray, engine and app keep running.
  The dock icon is hidden whenever no window is visible and shown while one is. `window-all-closed`
  does nothing.
- **Quit is explicit and the only way the app and its engine end**: tray "Quit Let's Triage Mail",
  Cmd+Q, logout/shutdown, or SIGTERM to the app. All go through `before-quit`: prevent once,
  `await engine.stop()`, then quit for real. A second quit request while stopping is ignored.
- Launching a second copy of the app focuses the first (`requestSingleInstanceLock`, `second-instance`
  opens the window). This also prevents two desktop-owned engines.
- **Autostart of the engine on app launch is off in this increment** (`LTM_AUTOSTART=1` or
  `--autostart` turns it on; the smoke test uses it). Tray has explicit "Start triage" / "Stop triage".
  Reason: the engine has no single-instance lock yet, so a launchd agent plus a desktop-started engine
  would both watch the same mailbox. This increment does not stop that at the OS level, but the desktop
  guards the click: a Start (tray or popup) first reads a fresh `status`, and if it reports a running
  engine the app did not spawn, no second one starts and the popup says "Running outside the app" with
  Start disabled. Target behaviour once the lock lands: autostart on, because the
  product is "closing the window leaves triage running".

Known limitation, documented: if the Electron main process is killed hard, the engine child is
orphaned. Fixed in a later increment (engine exits when its stdin closes).

## Window and tray security stance

- Tray: the design's menu-bar **template** icon (`assets/menubarTemplate.png`, 18 px, with a 36 px
  `@2x` beside it). A file named `*Template.png` is treated as a mask, so macOS tints the glyph for a
  light or dark menu bar: there is no light/dark pair to manage and no text label (a `setTitle` would
  put letters in the menu bar). Left-click toggles the popup; right-click pops up the native menu
  (status line; Start/Stop triage enabled by engine state; Check setup; Open window; disabled app-name
  and Engine version lines; Quit with Cmd+Q) — no persistent context menu.
- Popup: a second page, opened by that left-click, frameless, 360 px wide, its height sized to its
  content (measured by the main process, capped to the work area of the display that holds the icon),
  centred under the glyph and clamped inside that display's work area — it opens on the display whose
  icon was clicked. It hides on blur, like a native menu. The user cannot resize or move it: no dock
  handling, and its strings are built in the **main** process (`desktop/src/popup-model.ts`), so the
  page never formats or colours anything itself.
- Window: one `BrowserWindow`, default native title bar (the `hiddenInset` treatment comes with the
  design import, since design decisions stay with the board). Created on first open, then hidden/shown,
  never destroyed. Content is a static placeholder: engine state, the `check` result list, and
  Start/Stop/Check buttons, all through a tiny preload API. It is **not** the accounts/folders/settings
  screens.
- Security defaults, all required, on **both** pages: `contextIsolation: true`, `sandbox: true`,
  `nodeIntegration: false`, CSP `default-src 'none'; script-src 'self'; style-src 'self'`, `will-navigate`
  and `setWindowOpenHandler` deny everything, IPC handlers check `event.senderFrame.url` is one of **our
  two files** (the window page and the popup page), and the preload (see `desktop/src/preload.cjs`)
  exposes five functions only (`getState`, `onState`, `onCheckResult`, `getStatus`,
  `command(start|stop|check|openWindow|quit)`). A `getStatus` call from a hidden popup is answered
  `idle`: an invisible page never spawns the engine. The renderer and the popup write engine text with
  `textContent`, never `innerHTML` (check messages carry server and folder names, and the popup shows
  config account names), and a pill's colour class can only be one of four allow-listed names.

## Developer guide

Build the engine, then run the app against it:

```sh
cargo build                 # produces target/debug/lets-triage-mail
cd desktop
npm ci
npm start                   # builds, then `electron .`
```

Environment variables:

- `LTM_ENGINE_PATH` — a specific engine binary (a debug build in development).
- `LTM_CONFIG` — a config file; forwarded to the engine as a global `--config`. Unset means the
  engine's default config and database paths.
- `LTM_AUTOSTART=1` — start the engine on app launch (the smoke test sets this).
- `LTM_SMOKE=1` — run the executable smoke check (see Tests).

Engine lookup order: `LTM_ENGINE_PATH`, then a packaged `<resources>/engine/lets-triage-mail`
(implemented, nothing ships there yet), then `../target/debug/lets-triage-mail`.

Popup height is read with `webContents.executeJavaScript`: a **main**-initiated, read-only expression
that returns one number after each status paint, then applied with `setBounds`. An embedder-initiated
script is not subject to the page's CSP, so this adds no preload function and no IPC channel; if a
measurement ever fails the popup just keeps its last size (the next 5 s poll retries).

## Implemented, mockup only, future

Keeping these three apart is the point: it stops this document from reading like a wish list, and
`desktop/test/boundaries.test.ts` checks the three headings exist.

### Implemented

- A tray item with the design's template icon (tinted by macOS) and a native menu on right-click
  (status line; Start/Stop triage enabled by engine state; Check setup; Open window; disabled app-name
  and Engine version lines; Quit with Cmd+Q).
- The menu-bar **popup**: left-click opens it on the clicked display and on the macOS desktop (Space)
  the user is looking at, centred under the glyph and sized to its content (the main process measures
  the page), a blur closes it, and its content is
  `status --json` turned into ready-to-print strings in the main process (a generated date/time line
  above the counts, the big "to look at" number, `N fyi · M done · since HH:MM`, `X of Y connected`,
  one row per account with a state pill). Its **Start** is disabled, with the note "Running outside the
  app", when a fresh `status` shows a live engine the app did not start (there is no single-instance
  lock yet); its **Check setup** opens the main window and pushes the report there through the same
  path the tray uses, rather than discarding it.
- Engine status for the UI: `status --json` (shape locked by a Rust test), fed by two SQLite tables the
  engine writes (`engine_status` heartbeat + `account_status` per account). No socket, no port.
- A single window that hides on close and reopens by reuse (never destroyed); the dock hides when no
  window is visible.
- The engine start/stop/quit lifecycle: one SIGTERM then SIGKILL after the deadline, single-instance
  lock, explicit quit through `before-quit`.
- `check --json` parsing and rendering as data only (`textContent`), with a defensive shape check.
- Engine version display in the tray menu.

### Mockup only (designed in the private design repo, nothing coded)

- Accounts list and "add account" (`TriageMailAccounts`).
- Per-account server folders with modes `off/report/classify/act` and actions (`TriageMailFolders`),
  matching `docs/SPEC.md` §5 and needing engine v1.1a/b first.
- Settings: general, notifications, triage (`TriageMailSettings`).
- The desktop-controls decision (the Switch, gear/pause icons, `AppBar inset`, menu-bar emblem).

The design files are named only; nothing here reproduces or links their contents.

### Future

Everything in the "Next increments" table and "Distribution and signing" section below.

## Next increments

| # | Increment | Gate |
|---|---|---|
| 2 | Engine safe to embed: single-instance lock on `run`, exit when stdin closes, desktop restart policy with backoff, then flip autostart on | `File::lock` is stable only from Rust 1.89 and `rust-version` is 1.85: needs an MSRV bump or a crate. **Board decision.** |
| 3 | **Done by this PR.** Engine status for the UI: `status --json` with per-account state and counts, from two SQLite tables the engine writes | No channel design needed after all: the database the engine already owns is the channel, so `node:net` stays out of the boundary test. A 0600 Unix socket remains available as a later increment. |
| 4 | Config through the engine: `config show/validate --json` and engine-owned set/add/remove that keep comments and never log passwords; Keychain entry creation | Needs a TOML-editing crate: **board approval**. Prerequisite for 5–7. |
| 5 | **Done by this PR** (the tray part). Tray popover from `TriageMailTray` + design import | Board must approve publishing each design asset publicly; the popup is plain HTML/TS, so no UI framework was decided. |
| 6 | Accounts screen (add account, test sign-in via `check --account`, password to Keychain, pause/resume) | Needs 4. |
| 7 | Folders screen | Needs engine v1.1a folder discovery and v1.1b modes first. |
| 8 | Settings screen (general, notifications, triage) | Needs 4. Notifications no longer need 3: `status --json` already carries what a balloon would say. |
| 9 | Packaging: bundle the engine into the app, locally signed (ad-hoc) dev build, launch at login | Packager choice recorded in `docs/DECISIONS.md`. |
| 10 | Release pipeline and updates: Developer ID signing, notarization, DMG/zip on GitHub Releases, optional update check, Homebrew cask | Board: Apple Developer Program, CI secrets, public release. Adds the privacy statement. |

## Distribution and signing requirements (future)

- Apple Developer ID certificate and **notarization** (`notarytool`); hardened runtime with an
  entitlements file; **sign the bundled engine binary itself** before the app, and the Electron helper
  apps. Certificates and notarization credentials live in GitHub encrypted secrets, set up by the board.
  Until then: unsigned builds need right-click Open; do not ship them as a release.
- Engine build: `aarch64-apple-darwin` first (Apple Silicon); decide `x86_64` or a universal binary;
  Electron arch must match. The engine already uses `rusqlite` bundled and rustls, so no system
  libraries beyond the OS.
- **Not the Mac App Store**: the sandbox forbids spawning arbitrary `password_command` programs and a
  bundled daemon. Direct distribution only.
- Launch at login: `app.setLoginItemSettings` or SMAppService; must replace, not coexist with, the
  LaunchAgent (needs increment 2's lock).
- Updates: Squirrel.Mac needs a signed app; the update check is the first network call the desktop
  makes and **must** add a privacy statement, an off switch and no telemetry.
- Keychain: `password_command` runs `security`; a Finder-launched app has a minimal `PATH`, so user-chosen
  password tools in Homebrew paths will not resolve — the settings screen should accept an absolute path
  or the engine should extend `PATH`.
- Version drift: `Cargo.toml` and `desktop/package.json` versions must match; the app refuses a bundled
  engine whose `--version` differs (a cheap check in increment 9).
