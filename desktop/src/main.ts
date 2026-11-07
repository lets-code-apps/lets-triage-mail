// Wiring only: single-instance lock, dock, tray, the controllers and the quit choreography. No
// triage logic lives here — every engine fact comes from EngineController, which shells out to
// `lets-triage-mail`. The renderer talks back through the five preload functions only, the popup
// page through `getStatus` and `command`.
//
// The whole app ends only through an explicit quit (tray "Quit", Cmd+Q, or SIGTERM to the app);
// closing the window just hides it (docs/DESKTOP.md). The popup is a second, read-only surface:
// left-click on the tray icon opens it, a blur closes it, and it never touches the dock.

import { app, BrowserWindow, Menu, Tray, ipcMain, screen } from 'electron';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

import { EngineController, defaultSpawn } from './engine.ts';
import { WindowController } from './window.ts';
import { buildPopupView } from './popup-model.ts';
import { POPUP_SIZE, popupPosition } from './popup.ts';
import { PopupController } from './popup.ts';
import { buildTrayMenu } from './menu.ts';
import {
	CHANNEL,
	IDLE_PAYLOAD,
	checkPayload,
	statusPayload,
	type CommandResult,
	type EngineCommand,
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
		width: 640,
		height: 480,
		webPreferences: { preload: join(HERE, 'preload.cjs'), contextIsolation: true, sandbox: true, nodeIntegration: false },
	});
	browser.webContents.on('will-navigate', (event: { preventDefault(): void }) => event.preventDefault());
	browser.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
	browser.on('close', (event) => ctl.handleClose(event));
	browser.loadFile(RENDERER);
	currentWindow = browser;
	return browser;
}

// The popup: a frameless, fixed-size page under the tray icon. `show: false` because the controller
// decides when it appears; it is never resized, moved or given a menu of its own, and it never
// touches the dock (a popup opening must not make the app icon appear).
function makePopupWindow(): BrowserWindow {
	const bounds = tray?.getBounds() ?? { x: 0, y: 0, width: 0, height: 0 };
	const area = screen.getPrimaryDisplay().workArea;
	const at = popupPosition(bounds, area, POPUP_SIZE);
	const browser = new BrowserWindow({
		x: at.x,
		y: at.y,
		width: POPUP_SIZE.width,
		height: POPUP_SIZE.height,
		frame: false,
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
	return browser;
}

// The popup window's owner. `Date.now` is its clock: a tray click within 250 ms of our own blur-hide
// is the same gesture, not a new one (popup.ts explains why macOS forces this).
const popupCtl = new PopupController(() => makePopupWindow(), Date.now);

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
	tray = makeTray();
	// Left-click toggles the popup; right-click shows the native menu. No persistent context
	// menu, so the menu bar shows only the template glyph.
	tray.on('click', () => popupCtl.trayClick(Date.now()));
	tray.on('right-click', () => tray?.popUpContextMenu(trayMenu()));
	void engine.readVersion().then(() => {
		engine.onState((view) => {
			currentWindow?.webContents.send(CHANNEL.state, view);
		});
	});
	app.dock?.hide(); // menu-bar companion: no dock icon until a window is shown
	app.on('window-all-closed', () => ctl.handleWindowAllClosed());

	if (process.env.LTM_AUTOSTART === '1') engine.start();

	ipcMain.handle(CHANNEL.getState, (event) => (fromUs(event.senderFrame) ? engine.getState() : null));
	// The popup's poll. A hidden popup gets `idle` instead of an engine spawn: polling stops with
	// the window rather than with a timer, and an invisible page never touches the database.
	ipcMain.handle(CHANNEL.getStatus, async (event): Promise<StatusPayload> => {
		if (!fromUs(event.senderFrame) || !popupCtl.isVisible()) return IDLE_PAYLOAD;
		const result = await engine.status();
		if (result.kind === 'error') return { kind: 'error', message: result.message };
		return statusPayload(result, buildPopupView(result.report, {
			...clockOptions(),
			ownsEngine: appOwnsEngine(),
		}));
	});
	ipcMain.handle(CHANNEL.command, async (event, command: EngineCommand) => {
		if (!fromUs(event.senderFrame)) {
			return { command, view: engine.getState(), error: 'sender not allowed' } satisfies CommandResult;
		}
		if (command === 'start') await performStart();
		else if (command === 'stop') await engine.stop();
		else if (command === 'openWindow') ctl.open();
		else if (command === 'quit') {
			// The popup's Quit button: the same path as the tray item and Cmd+Q.
			app.quit();
			return { command, view: engine.getState() } satisfies CommandResult;
		} else if (command === 'check') {
			// The popup has no room for a full report and no `onCheckResult` sink of its own, so
			// its "Check setup" reuses the tray path: open the main window and push the finished
			// check there through `deliverCheck`. Only the window page keeps the report inline.
			if (event.senderFrame?.url === POPUP_URL) {
				await deliverCheck();
				return { command, view: engine.getState() } satisfies CommandResult;
			}
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
