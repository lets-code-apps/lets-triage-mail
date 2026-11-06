// Wiring only: single-instance lock, dock, tray, the controllers and the quit choreography. No
// triage logic lives here — every engine fact comes from EngineController, which shells out to
// `lets-triage-mail`. The renderer talks back through the three preload functions only.
//
// The whole app ends only through an explicit quit (tray "Quit", Cmd+Q, or SIGTERM to the app);
// closing the window just hides it (docs/DESKTOP.md).

import { app, BrowserWindow, Menu, Tray, ipcMain, nativeImage } from 'electron';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { writeFileSync } from 'node:fs';

import { EngineController, defaultSpawn } from './engine.ts';
import { WindowController } from './window.ts';
import { buildTrayMenu } from './menu.ts';
import { CHANNEL, type CommandResult } from './ipc.ts';

// ESM has no __dirname; derive the directory this module lives in once.
const HERE = import.meta.dirname;
const RENDERER = join(HERE, '..', 'renderer', 'index.html');
const RENDERER_URL = pathToFileURL(RENDERER).href;
const engine = new EngineController(process.env, join(HERE, '..', '..'), defaultSpawn);

let currentWindow: BrowserWindow | null = null;
const ctl = new WindowController(() => makeWindow(), {
	hide: () => app.dock?.hide(),
	show: () => app.dock?.show(),
});
let tray: Tray | undefined;
let quitInFlight = false;

// Only messages from our own renderer file are trusted; anything else is refused.
const fromUs = (senderFrame?: { url?: string } | null): boolean => senderFrame?.url === RENDERER_URL;

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

function refreshTrayMenu(): void {
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
	tray?.setContextMenu(Menu.buildFromTemplate(template));
}

function makeTray(): Tray {
	// Placeholder only: an empty image plus a title. The real template icon is a private design asset.
	const png = join(app.getPath('temp'), 'ltm-tray-placeholder.png');
	writeFileSync(png, nativeImage.createEmpty().toPNG());
	const created = new Tray(png);
	created.setTitle('LTM');
	return created;
}

function runAction(action: 'start' | 'stop' | 'check' | 'openWindow' | 'quit'): void {
	switch (action) {
		case 'start':
			engine.start();
			break;
		case 'stop':
			void engine.stop();
			break;
		case 'check':
			void engine.runCheck();
			break;
		case 'openWindow':
			ctl.open();
			break;
		case 'quit':
			app.quit();
			break;
	}
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
	void engine.readVersion().then(() => {
		refreshTrayMenu();
		engine.onState((view) => {
			refreshTrayMenu();
			currentWindow?.webContents.send(CHANNEL.state, view);
		});
	});
	app.dock?.hide(); // menu-bar companion: no dock icon until a window is shown
	app.on('window-all-closed', () => ctl.handleWindowAllClosed());

	if (process.env.LTM_AUTOSTART === '1') engine.start();

	ipcMain.handle(CHANNEL.getState, (event) => (fromUs(event.senderFrame) ? engine.getState() : null));
	ipcMain.handle(CHANNEL.command, async (event, command: 'start' | 'stop' | 'check') => {
		if (!fromUs(event.senderFrame)) {
			return { command, view: engine.getState(), error: 'sender not allowed' } satisfies CommandResult;
		}
		if (command === 'start') engine.start();
		else if (command === 'stop') await engine.stop();
		else {
			const result = await engine.runCheck();
			return {
				command,
				view: engine.getState(),
				report: result.kind === 'report' ? result.report : null,
				error: result.kind === 'error' ? result.message : null,
			} satisfies CommandResult;
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
		// 2. Quit must stop the engine and end the app within the 5s deadline (exit 0).
		app.on('will-quit', () => {
			clearTimeout(deadline);
			console.error('SMOKE OK: window hid, engine ran, quit sent SIGTERM, exit 0');
		});
		app.quit();
	};
	waitForRunning(0);
}
