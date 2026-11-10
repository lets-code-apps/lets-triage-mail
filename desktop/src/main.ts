// Wiring only: single-instance lock, dock, tray, the controllers and the quit choreography. No
// triage logic lives here — every engine fact comes from EngineController, which shells out to
// `lets-triage-mail`. The renderer talks back through the five preload functions only, the popup
// page through `getStatus` and `command`.
//
// The whole app ends only through an explicit quit (tray "Quit", Cmd+Q, or SIGTERM to the app);
// closing the window just hides it (docs/DESKTOP.md). The popup is a second, read-only surface:
// left-click on the tray icon opens it, a blur closes it, and it never touches the dock.

import { app, BrowserWindow, Menu, Tray, ipcMain, nativeTheme, screen, systemPreferences } from 'electron';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

import { EngineController, defaultSpawn } from './engine.ts';
import { WindowController } from './window.ts';
import { buildPopupView } from './popup-model.ts';
import { POPUP_FALLBACK_HEIGHT, POPUP_WIDTH, type PopupAnchor, type PopupHandle } from './popup.ts';
import { PopupController } from './popup.ts';
import { buildTrayMenu } from './menu.ts';
import { DEFAULT_SETTINGS, isAllowedSetting, parseSettings, type Settings } from './settings.ts';
import {
	CHANNEL,
	IDLE_PAYLOAD,
	checkPayload,
	statusPayload,
	type CommandResult,
	type EngineCommand,
	type SettingsPayload,
	type StatusPayload,
} from './ipc.ts';

// ESM has no __dirname; derive the directory this module lives in once.
const HERE = import.meta.dirname;
const RENDERER = join(HERE, '..', 'renderer', 'index.html');
const RENDERER_URL = pathToFileURL(RENDERER).href;
const POPUP = join(HERE, '..', 'renderer', 'popup.html');
const POPUP_URL = pathToFileURL(POPUP).href;
// The design's menu-bar template: the `Template` suffix plus `setTemplateImage` is what lets macOS
// tint the glyph for a light or dark menu bar.
const TRAY_ICON = join(HERE, '..', 'assets', 'menubarTemplate.png');
const engine = new EngineController(process.env, join(HERE, '..', '..'), defaultSpawn);

let currentWindow: BrowserWindow | null = null;
const ctl = new WindowController(() => makeWindow(), {
	hide: () => app.dock?.hide(),
	show: () => app.dock?.show(),
});
let tray: Tray | undefined;
let quitInFlight = false;

// The app's own settings (appearance only): one small JSON file in userData. `nativeTheme` makes
// both pages follow it through prefers-color-scheme. Login-item state is macOS's, not ours.
let settings: Settings = DEFAULT_SETTINGS;
let settingsFile = '';
// One-shot: the popup's settings button asks the window to open on settings; the window page picks
// this up through getSettings (on focus) and main clears it.
let pendingSection: 'settings' | null = null;

function settingsPayload(): SettingsPayload {
	const section = pendingSection;
	pendingSection = null;
	return {
		appearance: settings.appearance,
		openAtLogin: app.getLoginItemSettings().openAtLogin,
		log: engine.getLog(),
		section,
	};
}

// Only messages from our own two pages are trusted; anything else is refused. The popup is the
// second URL: `getStatus` and `command` both come from here or not at all.
const fromUs = (senderFrame?: { url?: string } | null): boolean =>
	senderFrame?.url === RENDERER_URL || senderFrame?.url === POPUP_URL;

// The clock the popup's strings are built with: the user's locale and zone, pinned once per
// read rather than guessed by the page (see popup-model.ts).
const clockOptions = () => ({
	locale: app.getLocale(),
	timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
});

// Whether the live engine is the one this app spawned. A fresh `status` that reports `running`
// while this is false means something else started it (a launchd agent or a terminal). There is no
// single-instance lock yet, so the popup must not offer to start a second watcher over the same
// mailbox; the real fix is increment 2 (docs/DESKTOP.md, risk "No single-instance lock yet").
const appOwnsEngine = (): boolean => {
	const state = engine.getState().state;
	return state === 'running' || state === 'starting';
};

function makeWindow(): BrowserWindow {
	const browser = new BrowserWindow({
		width: 1100,
		height: 760,
		// nav 200 + list 420 + detail 420: the panes never stack, the window just stops shrinking.
		minWidth: 1040,
		minHeight: 640,
		// The macOS buttons are ~14 px tall; centred in the 64 px app bar that is y = (64 - 14) / 2 = 25.
		trafficLightPosition: { x: 20, y: 25 },
		titleBarStyle: 'hiddenInset',
		webPreferences: { preload: join(HERE, 'preload.cjs'), contextIsolation: true, sandbox: true, nodeIntegration: false },
	});
	browser.webContents.on('will-navigate', (event: { preventDefault(): void }) => event.preventDefault());
	browser.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
	browser.on('close', (event) => ctl.handleClose(event));
	browser.loadFile(RENDERER);
	currentWindow = browser;
	return browser;
}

// The popup: a frameless page hung under the tray icon. It is created at the fallback size only;
// PopupController sizes and places it on every open (and re-sizes it after every poll while it is
// visible), so it lands on the display whose icon was clicked and is tall enough for every row.
// `show: false` because the controller decides when it appears; the user cannot resize or move it,
// and opening a popup must not make the app icon appear (the dock belongs to the window controller).
let popupWindow: BrowserWindow | null = null;

function makePopupWindow(): BrowserWindow {
	const browser = new BrowserWindow({
		width: POPUP_WIDTH,
		height: POPUP_FALLBACK_HEIGHT,
		frame: false,
		// Transparent so the card's rounded corners are not squared off by a window background.
		transparent: true,
		resizable: false,
		movable: false,
		show: false,
		webPreferences: { preload: join(HERE, 'preload.cjs'), contextIsolation: true, sandbox: true, nodeIntegration: false },
	});
	browser.webContents.on('will-navigate', (event: { preventDefault(): void }) => event.preventDefault());
	browser.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
	// Clicking anywhere else closes the popup, like a native menu (see popup.ts).
	browser.on('blur', () => popupCtl.handleBlur());
	// Closing is a hide, except while quitting, when the window really must go away.
	browser.on('close', (event) => {
		if (quitInFlight) return;
		event.preventDefault();
		browser.hide();
	});
	browser.loadFile(POPUP);
	popupWindow = browser;
	return browser;
}

// The popup's view of itself as a `PopupHandle`: `popup.ts` decides *when* the window follows the
// desktop the user is looking at, and this is where Electron's macOS-only call lives. Claiming
// it once per window rather than on every open matters: the docs state that each
// `setVisibleOnAllWorkspaces` call transforms the process type between UIElementApplication and
// ForegroundApplication, briefly hiding the window and the Dock icon. `visibleOnFullScreen` keeps
// its default (false), so the popup never rides on top of a full-screen app on another desktop.
const asPopupHandle = (browser: BrowserWindow): PopupHandle => ({
	isVisible: () => browser.isVisible(),
	show: () => browser.show(),
	hide: () => browser.hide(),
	setBounds: (bounds) => browser.setBounds(bounds),
	focus: () => browser.focus(),
	followCurrentSpace: () => {
		if (browser.isVisibleOnAllWorkspaces()) return; // already claimed: claiming twice costs a flicker
		browser.setVisibleOnAllWorkspaces(true);
	},
});

// Where to hang the popup: the tray icon's rectangle, and the work area of the display nearest the
// icon's centre. With "separate Spaces" macOS runs one menu bar per display, so the rectangle the
// click landed on also names the display (and its coordinate space, negative x/y to the left or
// above the primary) the popup must live in. The controller calls this on every open, so the popup
// follows the display the user clicks instead of sticking to the first one.
function popupAnchor(): PopupAnchor {
	const icon = tray?.getBounds() ?? { x: 0, y: 0, width: 0, height: 0 };
	const display = screen.getDisplayNearestPoint({ x: icon.x + icon.width / 2, y: icon.y + icon.height / 2 });
	return { tray: icon, workArea: display.workArea };
}

// The popup window's owner: it owns creation, placement, sizing and the grace period. `Date.now`
// is its clock: a tray click within 250 ms of our own blur-hide is the same gesture, not a new one
// (popup.ts explains why macOS forces this).
const popupCtl = new PopupController(() => asPopupHandle(makePopupWindow()), popupAnchor, Date.now);

// The popup's height follows its content, measured from the **main** side: one read-only
// expression in the page, one number back. An embedder-initiated `executeJavaScript` script is
// not governed by the page's CSP, so this adds no preload function and no IPC channel. It waits
// two animation frames (the page has painted the reply that just arrived by then) and reports the
// `#popup` box plus the body's default 8 px margins — the real content height includes them,
// and leaving them out would reintroduce a scrollbar.
const POPUP_MEASURE =
	"new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => {" +
	"const box = document.getElementById('popup');" +
	"const body = getComputedStyle(document.body);" +
	"resolve(box.getBoundingClientRect().height + parseFloat(body.marginTop) + parseFloat(body.marginBottom));" +
	"})));";

// Measure an open popup after a status paint and hand the height to the controller. A hidden
// window does not run animation frames, so this runs only while the popup is visible; a failed
// measure is ignored (the window keeps its last size) and the next poll retries it.
function fitPopup(): void {
	if (!popupCtl.isVisible() || popupWindow === null) return;
	void popupWindow.webContents
		.executeJavaScript(POPUP_MEASURE)
		.then((height: unknown) => popupCtl.setContentHeight(Number(height)))
		.catch(() => undefined);
}

// The tray menu, built fresh on right-click. No persistent context menu: macOS shows the tray
// glyph alone until the user asks for the list.
function trayMenu(): Menu {
	const template = buildTrayMenu(engine.getState()).map((item) => {
		const built: { label: string; enabled: boolean; click?: () => void; accelerator?: string } = {
			label: item.label,
			enabled: item.enabled,
		};
		const action = item.action;
		if (action) built.click = () => runAction(action);
		if (item.accelerator) built.accelerator = item.accelerator;
		return built;
	});
	return Menu.buildFromTemplate(template);
}

function makeTray(): Tray {
	// The design's template glyph (18x18, with a 36x36 @2x beside it). The `Template` suffix is the
	// whole contract: Electron's Tray treats such a file as a mask and macOS tints it for a light or
	// dark menu bar, so there is no light/dark pair to manage. `setTitle` is deliberately absent:
	// it would put the letters "LTM" in the menu bar next to the glyph.
	return new Tray(TRAY_ICON);
}

function runAction(action: EngineCommand): void {
	switch (action) {
		case 'start':
			void performStart();
			break;
		case 'stop':
			void engine.stop();
			break;
		case 'check':
			void deliverCheck();
			break;
		case 'openWindow':
			ctl.open();
			break;
		case 'openSettings':
			pendingSection = 'settings';
			ctl.open();
			break;
		case 'quit':
			app.quit();
			break;
	}
}

// A Start is only allowed when no engine is already live. A fresh `status --json` is the arbiter:
// if it says `running` while this app did not spawn that process (appOwnsEngine() is false), starting
// another would double-watch the mailbox and let two runs overwrite each other's status rows. The
// popup disables its Start button on the same fact, so the user sees why before they can click it.
async function performStart(): Promise<void> {
	const result = await engine.status();
	const external = result.kind === 'status' && result.report.engine.running && !appOwnsEngine();
	if (!external) engine.start();
}

// The tray "Check setup" opens the window (so there is somewhere to show the result) and then
// pushes the finished check to the renderer as data. It waits for the page to finish loading before
// sending, otherwise the message races the renderer and is dropped.
function deliverCheck(): Promise<void> {
	ctl.open();
	const browser = currentWindow;
	if (browser === null) return Promise.resolve();
	return engine.runCheck().then((result) => {
		const payload = checkPayload('check', engine.getState(), result);
		const send = (): void => browser.webContents.send(CHANNEL.checkResult, payload);
		if (browser.webContents.isLoading()) browser.webContents.once('did-finish-load', send);
		else send();
	});
}

app.on('before-quit', (event) => {
	ctl.beginQuitting();
	if (quitInFlight) return; // a second quit request while stopping is ignored
	quitInFlight = true;
	event.preventDefault(); // halt here; quit for real once the engine is down
	void engine.stop().then(() => app.quit());
});

if (!app.requestSingleInstanceLock()) {
	app.quit();
} else {
	app.on('second-instance', () => {
		ctl.open();
	});
	void app.whenReady().then(boot);
}

function boot(): void {
	settingsFile = join(app.getPath('userData'), 'settings.json');
	try {
		settings = parseSettings(readFileSync(settingsFile, 'utf8'));
	} catch {
		// no file yet: defaults
	}
	nativeTheme.themeSource = settings.appearance;
	tray = makeTray();
	// Left-click toggles the popup; right-click shows the native menu. No persistent context
	// menu, so the menu bar shows only the template glyph.
	tray.on('click', () => popupCtl.trayClick(Date.now()));
	tray.on('right-click', () => {
		popupCtl.close(); // no grace period: the menu must not open on top of a lingering popup
		tray?.popUpContextMenu(trayMenu());
	});
	// Our app stopped being the active one (a click on another app or the desktop): close like a menu.
	app.on('did-resign-active', () => popupCtl.handleBlur());
	// Another menu-bar item opened its menu (the clock, Wi-Fi, another app's tray menu). Those clicks
	// don't activate their app, so neither blur nor did-resign-active fires; macOS broadcasts this
	// distributed notification whenever any menu starts tracking, which is how native menus close
	// each other. Items that open a popover window instead of a menu may still not send it.
	systemPreferences.subscribeNotification('com.apple.HIToolbox.beginMenuTrackingNotification', () => popupCtl.handleBlur());
	void engine.readVersion().then(() => {
		// Both pages show the engine state (the popup's pause/start button and status line).
		engine.onState((view) => {
			currentWindow?.webContents.send(CHANNEL.state, view);
			popupWindow?.webContents.send(CHANNEL.state, view);
		});
	});
	app.dock?.hide(); // menu-bar companion: no dock icon until a window is shown
	app.on('window-all-closed', () => ctl.handleWindowAllClosed());

	if (process.env.LTM_AUTOSTART === '1') engine.start();

	ipcMain.handle(CHANNEL.getState, (event) => (fromUs(event.senderFrame) ? engine.getState() : null));
	ipcMain.handle(CHANNEL.getSettings, (event) => (fromUs(event.senderFrame) ? settingsPayload() : null));
	// Validated at the trust boundary: our own page, a known name, a value from the allow-list.
	ipcMain.handle(CHANNEL.setSetting, (event, name: unknown, value: unknown) => {
		if (!fromUs(event.senderFrame) || !isAllowedSetting(name, value)) return null;
		if (name === 'openAtLogin') app.setLoginItemSettings({ openAtLogin: value as boolean });
		else {
			settings = { appearance: value as Settings['appearance'] };
			nativeTheme.themeSource = settings.appearance;
			try {
				writeFileSync(settingsFile, JSON.stringify(settings));
			} catch {
				// not persisted this time; the choice still applies until quit
			}
		}
		return settingsPayload();
	});
	// The popup's and the window's poll. A hidden page gets `idle` instead of an engine spawn:
	// polling stops with the surface rather than with a timer, and an invisible page never touches
	// the database. Each page is gated on its own surface being visible.
	ipcMain.handle(CHANNEL.getStatus, async (event): Promise<StatusPayload> => {
		if (!fromUs(event.senderFrame)) return IDLE_PAYLOAD;
		const fromPopup = event.senderFrame?.url === POPUP_URL;
		if (!(fromPopup ? popupCtl.isVisible() : currentWindow?.isVisible())) return IDLE_PAYLOAD;
		const result = await engine.status();
		// Build the reply through `statusPayload` whether the engine returned a report or an error,
		// so the typed handler can neither drop the view nor forget the error. Both are then re-measured
		// like any other visible reply: an error paint clears the account rows the last status left
		// behind, so a stale tall window would show an empty band, and a long first error must grow
		// past the 260 px fallback instead of clipping. Neither an error nor a status may return
		// ahead of the measure.
		const payload: StatusPayload = result.kind === 'status'
			? statusPayload(result, buildPopupView(result.report, {
				...clockOptions(),
				ownsEngine: appOwnsEngine(),
			}))
			: { kind: 'error', message: result.message };
		// Ask for a re-measure, deferred with `setImmediate` so this handler's return value reaches
		// the page first; the two-frame wait inside POPUP_MEASURE then covers the paint. The page
		// polls every 5 s while visible, so a missed measurement self-corrects within one poll.
		if (fromPopup) setImmediate(() => void fitPopup());
		return payload;
	});
	ipcMain.handle(CHANNEL.command, async (event, command: EngineCommand) => {
		if (!fromUs(event.senderFrame)) {
			return { command, view: engine.getState(), error: 'sender not allowed' } satisfies CommandResult;
		}
		if (command === 'start') await performStart();
		else if (command === 'stop') await engine.stop();
		else if (command === 'openWindow') {
			popupCtl.close();
			ctl.open();
		} else if (command === 'openSettings') {
			popupCtl.close();
			runAction('openSettings');
		}
		else if (command === 'quit') {
			// The popup's Quit button: the same path as the tray item and Cmd+Q.
			app.quit();
			return { command, view: engine.getState() } satisfies CommandResult;
		} else if (command === 'check') {
			const result = await engine.runCheck();
			return checkPayload(command, engine.getState(), result) satisfies CommandResult;
		}
		return { command, view: engine.getState() } satisfies CommandResult;
	});

	if (process.env.LTM_SMOKE === '1') void runSmoke();
}

// Executable proof that "closing the window does not end the background app, and Quit stops the
// engine". Needs a GUI session (CI macOS runners have one). Reports SMOKE OK / SMOKE FAIL.
function runSmoke(): void {
	const deadline = setTimeout(() => {
		console.error('SMOKE FAIL: timed out after 5s');
		app.exit(1);
	}, 5000);
	const fail = (message: string): never => {
		clearTimeout(deadline);
		console.error(`SMOKE FAIL: ${message}`);
		app.exit(1);
		throw new Error(message);
	};

	// 1. The engine (autostart on) must be running.
	const waitForRunning = (attempt: number): void => {
		if (engine.getState().state === 'running') void afterRunning();
		else if (attempt > 50) fail('engine never reached running');
		else setTimeout(() => waitForRunning(attempt + 1), 50);
	};
	const afterRunning = (): void => {
		ctl.open(); // tray-equivalent: open the window
		const browser = currentWindow ?? fail('no window after open');
		browser.close(); // red button / Cmd+W
		if (browser.isVisible()) fail('window still visible after close');
		if (engine.getState().state !== 'running') fail('engine stopped when the window closed');
		// 2. The popup's data path: one status read, parsed, and turned into ready-to-print strings.
		//    No window rendering is needed to prove the engine round-trip works end to end.
		void engine
			.status()
			.then((result) => {
				if (result.kind !== 'status') {
					fail('status --json did not produce a report');
					return; // unreachable: `fail` exits; the return only keeps the type narrow
				}
				const view = buildPopupView(result.report, clockOptions());
				if (view.headline.length === 0 || view.accounts.length === 0) fail('the popup view is empty');
				console.error(`SMOKE STATUS OK: ${view.summary} · ${view.subline}`);
				// 3. Quit must stop the engine and end the app within the 5s deadline (exit 0).
				app.on('will-quit', () => {
					clearTimeout(deadline);
					console.error('SMOKE OK: window hid, engine ran, status parsed, quit sent SIGTERM, exit 0');
				});
				app.quit();
			})
			.catch((error: unknown) => fail(`status read threw: ${String(error)}`));
	};
	waitForRunning(0);
}
