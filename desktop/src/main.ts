// Wiring only: single-instance lock, dock, tray, the controllers, the quit choreography and the
// first-run onboarding handler. No triage logic lives here — every engine fact comes from EngineController,
// which shells out to `lets-triage-mail`. The renderer talks back through the ten preload functions only
// (the tenth is `ltm.setup`), the popup page through `getStatus` and `command`.
//
// The whole app ends only through an explicit quit (tray "Quit", Cmd+Q, or SIGTERM to the app);
// closing the window just hides it (docs/DESKTOP.md). The popup is a second, read-only surface:
// left-click on the tray icon opens it, a blur closes it, and it never touches the dock.

import { app, BrowserWindow, Menu, Notification, Tray, dialog, ipcMain, nativeTheme, screen, shell, systemPreferences } from 'electron';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { totalmem } from 'node:os';

import { EngineController, CONFIG_TEST_TIMEOUT_MS, defaultSpawn, type ModelPullResult } from './engine.ts';
import { buildEditRequests, parseConfigShow, toEditResult, type AccountOp, type ConfigView, type EditResult } from './account-edit.ts';
import {
	accountNameFor,
	decideSetup,
	guessServer,
	modeChanges,
	OLLAMA_INSTALL_URL,
	buildAccountSetArgs,
	buildConfigSetArgs,
	buildModelDetectArgs,
	buildModelPullArgs,
	buildModelSaveArgs,
	parseConfigGlobal,
	parseModelDetect,
	parsePullLine,
	pullProgress,
	recommendedModel,
	type GlobalView,
	type ModeChoice,
	type ModelDetect,
} from './setup.ts';
import { STEP_ORDER, modelView, stepFromDetect, stepView, type DigestStepData } from './setup-model.ts';
import { createNotifier } from './notifier.ts';
import { modelAnswer } from './check-report.ts';
import { WindowController } from './window.ts';
import { buildPopupView, type PopupView } from './popup-model.ts';
import { openMail } from './open-mail.ts';
import { POPUP_FALLBACK_HEIGHT, POPUP_WIDTH, type PopupAnchor, type PopupHandle } from './popup.ts';
import { PopupController } from './popup.ts';
import { buildTrayMenu } from './menu.ts';
import { DEFAULT_SETTINGS, isAllowedSetting, parseSettings, skippedInstallSettings, type Settings } from './settings.ts';
import {
	CHANNEL,
	IDLE_PAYLOAD,
	checkPayload,
	statusPayload,
	type CheckView,
	type CommandResult,
	type EngineCommand,
	type SettingsPayload,
	type StatusPayload,
	type StatusResult,
	type SetupResult,
	type SetupStep,
	type SetupView,
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
// The app icon (design: ember grain tile, white emblem). A packaged build gets it from its bundle;
// `npm start` runs inside Electron.app, so the dock would show Electron's icon without this.
const APP_ICON = join(HERE, '..', 'assets', 'icon.png');
const engine = new EngineController(process.env, join(HERE, '..', '..'), defaultSpawn);

let currentWindow: BrowserWindow | null = null;
const ctl = new WindowController(() => makeWindow(), {
	hide: () => app.dock?.hide(),
	show: () => app.dock?.show(),
});
let tray: Tray | undefined;
let quitInFlight = false;
// When the user pressed Stop in this session (epoch seconds), so the popup can say "paused at HH:MM"
// rather than a bare "not running". In memory only (board decision): after an app restart this is null,
// and the popup uses the time-free "Paused." sentence. `null` while the engine is running or has never
// been stopped here is what keeps a crash (`failed`) and "never started" reading as not paused.
let pausedAt: number | null = null;

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
		// The settings screen's notification rows and the onboarding flag (board decision 2026-11-27:
		// the two switches get rows so an install that skipped setup can still turn them on).
		setupDone: settings.setupDone,
		notifyReply: notifySwitches.notifyReply,
		notifyOffline: notifySwitches.notifyOffline,
	};
}

// Only messages from our own two pages are trusted; anything else is refused. The popup is the
// second URL: `getStatus` and `command` both come from here or not at all.
const fromUs = (senderFrame?: { url?: string } | null): boolean =>
	senderFrame?.url === RENDERER_URL || senderFrame?.url === POPUP_URL;
const firstLine = (failed: unknown): string => (failed instanceof Error ? failed.message : String(failed)).split('\n')[0];
// The config calls are for the window page alone.
const fromWindow = (senderFrame?: { url?: string } | null): boolean => senderFrame?.url === RENDERER_URL;

// --- first-run onboarding ---------------------------------------------------------
//
// One channel (`ltm.setup`) carries every step the window page drives, and only the window page may
// use it: the popup never drives setup. The page owns the step number; main owns what the engine
// said — the last `model detect` answer, one `model pull` at a time, and the settings it persists.
// Ready-to-print views come from setup-model.ts (pure), engine argv from setup.ts (closed sets), so
// no string a page typed can become a flag, a URL or a colour.

// The Mac's memory, read once: the engine sizes the recommended model against it, and the step quotes
// the same number, so a pull never tries to load more than the machine has.
const MEM_GB = Math.round(totalmem() / 1024 ** 3);

// Everything onboarding keeps in memory. Nothing here is written to disk except the settings file,
// and no mail or model content is ever stored: these are step facts the page re-asks for.
type SetupSession = {
	step: SetupStep;
	modeChoice: ModeChoice;
	globals: GlobalView;
	detect: ModelDetect | null; // the last `model detect --json` answer, or null before the first
	modelSaved: boolean; // `model save` wrote the shared file: the step is ready
	checkAnswer: string | null; // the model line of `check --json`, when it answered ("answered a test question")
	pullState: 'idle' | 'pulling' | 'done' | 'failed' | 'cancelled';
	pullModel: string;
	pull: { model: string; done: number; gb: number; pct: number } | null;
	pullError: string | null;
	handle: { cancel(): void; done: Promise<ModelPullResult> } | null;
};
let setupSession: SetupSession | null = null;

// The two notification switches the settings screen can flip (the board's rows). The same live object
// is handed to notifier.ts, which reads the switches at every poll, so a flip takes effect without
// rebuilding anything. Both default true; an install that skipped setup got false.
const notifySwitches = { notifyReply: true, notifyOffline: true };
const notifier = createNotifier({
	// Electron's Notification is built here and nowhere else (boundaries.test.ts keeps it that way):
	// notifier.ts only decides *whether* and *how many*, never the text beyond its fixed sentences.
	notify: (title, body) => {
		try {
			new Notification({ title, body }).show();
		} catch {
			// macOS refused (permission not granted yet): the switch stays as the user set it.
		}
	},
	now: () => Date.now(),
	get notifyReply() {
		return notifySwitches.notifyReply;
	},
	get notifyOffline() {
		return notifySwitches.notifyOffline;
	},
});

// The settings file, written whole: the spread keeps the keys another handler added (the board's
// switches and `setupDone`), so a write never drops them.
function persistSettings(): void {
	try {
		writeFileSync(settingsFile, JSON.stringify(settings));
	} catch {
		// not persisted this time; the choice still applies until quit
	}
}

// The config facts every step prefills from, in one read: the accounts (account-edit trims the engine's
// answer to its own typed view) and the global keys that view ignores.
async function readGlobalConfig(): Promise<{ config: ConfigView; globals: GlobalView }> {
	const raw = await engine.runConfig(['show', '--json']);
	return { config: parseConfigShow(raw), globals: parseConfigGlobal(raw) };
}

const accountNames = (config: ConfigView): string[] => (config.ok ? config.accounts.map((account) => account.name) : []);

// The model step's state, from main's own last answers: nothing is re-probed to paint a step, so the
// page cannot make the desktop hit a runtime by asking for a view.
function modelStepView(session: SetupSession): SetupView {
	const base = stepFromDetect(session.detect ?? { ok: true, runtimes: [], shared: null, error: null }, MEM_GB);
	if (session.pullState === 'pulling') {
		return modelView('downloading', { ...base.data, model: session.pullModel, pull: session.pull });
	}
	if (session.pullState === 'failed') {
		return modelView('failed', { ...base.data, model: session.pullModel, error: session.pullError ?? undefined });
	}
	if (session.modelSaved) {
		// Ready: the check line when `check --json` answered one ("answered a test question", the
		// board's answer), installed-and-saved otherwise. No new engine command exists for a test.
		return modelView('ready', { ...base.data, sharedModel: base.state === 'shared', checkAnswer: session.checkAnswer ?? undefined });
	}
	if (session.detect === null) {
		return modelView('detecting', { runtimeName: 'Ollama', url: 'http://localhost:11434', model: recommendedModel(MEM_GB * 1024 ** 3).model, memGb: MEM_GB });
	}
	// found / not-found / shared straight from the detect answer; a cancelled pull lands back here, so
	// the step offers the sized download again.
	return modelView(base.state, base.data);
}

function setupViewFor(step: SetupStep, config: ConfigView, globals: GlobalView, session: SetupSession): SetupView {
	switch (step) {
		case 'model':
			return modelStepView(session);
		case 'mode':
			return stepView({ step: 'mode', data: { choice: session.modeChoice } });
		case 'digest':
			return stepView({
				step: 'digest',
				data: {
					enabled: globals.digestEnabled,
					time: globals.digestTime,
					accounts: accountNames(config),
					notifyReply: notifySwitches.notifyReply,
					notifyOffline: notifySwitches.notifyOffline,
				} satisfies DigestStepData,
			});
		default: {
			return stepView({ step: 'accounts', data: { accounts: accountNames(config), missing: !config.ok && config.missing, error: config.ok ? null : config.error } });
		}
	}
}

// The setup's go-live confirm: the same native dialog as the accounts screen's, policy-wide wording
// (every configured account), cancel still the default button.
async function confirmGoLive(): Promise<boolean> {
	const options = {
		type: 'warning' as const,
		buttons: ['classify and sort', 'cancel'],
		defaultId: 1,
		cancelId: 1,
		message: 'Let triage flag your mail on the server?',
		detail: 'Classify and sort sets flag colours on every configured account. It never moves or deletes mail.',
	};
	const { response } = currentWindow ? await dialog.showMessageBox(currentWindow, options) : await dialog.showMessageBox(options);
	return response === 0;
}

// The accounts step's "add account": the engine's own `test` of the typed values first (nothing
// stored), and only a pass runs `add`. Both go through account-edit, so the password reaches the
// child's stdin and never argv. A fail paints the check items on the step and stores nothing.
async function addAccount(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	const address = typeof fields.address === 'string' ? fields.address : '';
	const guessed = guessServer(address);
	const host = typeof fields.host === 'string' && fields.host !== '' ? fields.host : guessed.host;
	const port = typeof fields.port === 'number' ? fields.port : guessed.port;
	// The design has no name field: derive it from the address, disambiguated against the config.
	const current = await readGlobalConfig();
	const editFields = {
		name: accountNameFor(address, accountNames(current.config)),
		host,
		port,
		tls: 'tls',
		username: address,
		password: typeof fields.password === 'string' ? fields.password : '',
	};
	const test = await editAccount('test', editFields);
	if (!test.ok) {
		return {
			kind: 'view',
			view: { ...setupViewFor('accounts', current.config, current.globals, session), check: test.report ? { ok: test.report.ok, items: test.report.items } : null },
		};
	}
	const added = await editAccount('add', editFields);
	if (!added.ok) return { kind: 'error', message: added.error ?? 'the engine refused the add' };
	const after = await readGlobalConfig();
	session.globals = after.globals;
	return { kind: 'view', view: setupViewFor('accounts', after.config, after.globals, session) };
}

// The mode step's writes: the per-account keys the board decision settled (classify only = flagging
// off; both modes live so the digest appends). The go-live confirm belongs to "classify and sort"
// alone, and declining it applies classify only instead of doing nothing.
async function applyModeChoice(session: SetupSession, choice: ModeChoice): Promise<SetupResult> {
	session.modeChoice = choice;
	const current = await readGlobalConfig();
	if (!current.config.ok) return { kind: 'error', message: current.config.error };
	for (const account of current.config.accounts) {
		for (const change of modeChanges(choice)) {
			const args = buildAccountSetArgs(account.name, change.key, change.value);
			if (args === null) return { kind: 'error', message: 'That account name cannot be changed here.' };
			const raw = await engine.runConfig(args);
			if (raw.ok !== true) return { kind: 'error', message: typeof raw.error === 'string' ? raw.error : 'the engine gave no answer' };
		}
	}
	// The engine reads its config only at start; restart the run this app owns, once.
	if (appOwnsEngine()) {
		await engine.stop();
		engine.start();
	}
	const after = await readGlobalConfig();
	session.globals = after.globals;
	return { kind: 'view', view: setupViewFor('mode', after.config, after.globals, session) };
}

async function applyMode(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	if (fields.choice !== 'classify-only' && fields.choice !== 'classify-and-sort') return { kind: 'error', message: 'Not a known mode.' };
	if (fields.choice === 'classify-and-sort' && !(await confirmGoLive())) {
		return await applyModeChoice(session, 'classify-only');
	}
	return await applyModeChoice(session, fields.choice as ModeChoice);
}

// `model detect` again (the step's "check again"), or the first probe. `--url` only when the user
// typed an address into the step's field; anything else is the engine's two defaults.
async function runDetect(session: SetupSession, url: unknown): Promise<SetupResult> {
	const args = buildModelDetectArgs(url === '' ? undefined : url);
	if (args === null) return { kind: 'error', message: 'The runtime address is not valid.' };
	session.detect = parseModelDetect(await engine.runModel(args));
	return { kind: 'view', view: modelStepView(session) };
}

// Start a pull through the runtime that answers. The stream's lines update the session, and the
// download keeps running while the user continues or closes the window (cancel is its own op).
async function startPull(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	const model = typeof fields.model === 'string' && fields.model !== '' ? fields.model : recommendedModel(MEM_GB * 1024 ** 3).model;
	const answering = session.detect?.runtimes.find((probe) => probe.reachable);
	const url = typeof fields.url === 'string' && fields.url !== '' ? fields.url : (answering?.url ?? undefined);
	// The runtime/URL/model this pull is for, remembered so a completed download can record exactly what
	// it downloaded (see the save-on-done below); a typed address wins over the answering runtime.
	const saveUrl = url ?? answering?.url ?? '';
	const saveRuntime = answering?.runtime;
	const args = buildModelPullArgs(model, url);
	if (args === null) return { kind: 'error', message: 'The model or address is not valid.' };
	session.pullModel = model;
	session.pullState = 'pulling';
	session.pull = { model, done: 0, gb: 0, pct: 0 };
	session.pullError = null;
	const handle = engine.modelPull(args, (line) => {
		const parsed = parsePullLine(line);
		if (parsed === null) return;
		if (parsed.stage === 'downloading') session.pull = pullProgress(model, parsed) ?? session.pull;
		else if (parsed.stage === 'error') session.pullError = parsed.text;
	});
	session.handle = handle;
	// A successful pull must also save the model it downloaded, or the finished download is never made
	// available to triage (the downloading view has no chooser, so the page's save-on-continue branch
	// cannot run). Save the runtime/URL/model captured above, then read the model part of `check --json`
	// for the ready proof — the same recording `saveModel` does.
	void handle.done.then(async (result) => {
		session.pull = null;
		if (result.cancelled) session.pullState = 'cancelled';
		else if (!result.ok) {
			session.pullState = 'failed';
			session.pullError = result.error ?? session.pullError;
		} else {
			session.pullState = 'done';
			const saveArgs = buildModelSaveArgs(saveRuntime, saveUrl, model);
			if (saveArgs !== null) {
				const raw = await engine.runModel(saveArgs);
				if (raw.ok === true) {
					session.modelSaved = true;
					// The saved model must reach a watcher that is already running (the autostart,
					// or a Continue pressed during the download, started it with the old choice):
					// reload the owned watcher, or triage keeps using the model from before (finding 3).
					await engine.reloadIfRunning();
					const check = await engine.runCheck();
					session.checkAnswer = check.kind === 'report' ? modelAnswer(check.report) : null;
				}
			}
		}
	});
	return { kind: 'view', view: modelStepView(session) };
}

// `model save` records the choice (shared_model.rs, whose first caller this is), then the model part
// of the engine's own `check --json` becomes the ready state's proof when it answers one.
async function saveModel(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	const answering = session.detect?.runtimes.find((probe) => probe.reachable);
	const shared = session.detect?.shared;
	const model = typeof fields.model === 'string' && fields.model !== '' ? fields.model : session.pullModel;
	const url = typeof fields.url === 'string' && fields.url !== '' ? fields.url : (answering?.url ?? shared?.url ?? '');
	const runtime = answering?.runtime ?? shared?.runtime;
	const args = buildModelSaveArgs(runtime, url, model);
	if (args === null) return { kind: 'error', message: 'The model choice is not valid.' };
	const raw = await engine.runModel(args);
	if (raw.ok !== true) return { kind: 'error', message: typeof raw.error === 'string' ? raw.error : 'the engine refused' };
	session.modelSaved = true;
	const check = await engine.runCheck();
	session.checkAnswer = check.kind === 'report' ? modelAnswer(check.report) : null;
	return { kind: 'view', view: modelStepView(session) };
}

// The digest step's writes: the two engine keys through the closed `config set` allow-list (the
// engine validates the value again), then a re-read paints the step.
async function setDigest(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	const write = async (key: string, value: string): Promise<string | null> => {
		const args = buildConfigSetArgs(key, value);
		if (args === null) return `${key} must be a ${key === 'digest.time' ? 'time like 08:00' : 'true or false'}`;
		const raw = await engine.runConfig(args);
		return raw.ok === true ? null : typeof raw.error === 'string' ? raw.error : 'the engine gave no answer';
	};
	if (typeof fields.enabled === 'boolean') {
		const failure = await write('digest.enabled', fields.enabled ? 'true' : 'false');
		if (failure !== null) return { kind: 'error', message: failure };
	}
	if (typeof fields.time === 'string') {
		const failure = await write('digest.time', fields.time);
		if (failure !== null) return { kind: 'error', message: failure };
	}
	const after = await readGlobalConfig();
	session.globals = after.globals;
	return { kind: 'view', view: setupViewFor('digest', after.config, after.globals, session) };
}

// The two notification switches: settings.json, not the engine (they are this app's own preference).
async function setNotifications(session: SetupSession, fields: Record<string, unknown>): Promise<SetupResult> {
	if (typeof fields.notifyReply === 'boolean') notifySwitches.notifyReply = fields.notifyReply;
	if (typeof fields.notifyOffline === 'boolean') notifySwitches.notifyOffline = fields.notifyOffline;
	settings = { ...settings, notifyReply: notifySwitches.notifyReply, notifyOffline: notifySwitches.notifyOffline };
	persistSettings();
	const after = await readGlobalConfig();
	return { kind: 'view', view: setupViewFor('digest', after.config, after.globals, session) };
}

// "start triage": the settings are final (setupDone written with them), then the engine starts. A
// first run may have it `failed` on "config missing"; start() spawns afresh from failed. The digest
// and notification writes just landed, and the engine reads its config only at start — so a watcher
// this app owns is reloaded first; `performStart` alone would no-op while it runs (round-2 finding 3).
async function finishSetup(session: SetupSession): Promise<SetupResult> {
	settings = { ...settings, setupDone: true };
	persistSettings();
	await engine.reloadIfRunning();
	await performStart();
	return { kind: 'done' };
}

// One op from the page. Unknown names answer an error, a foreign sender answers idle; every case
// re-answers with the step's view so the page needs no second round-trip.
async function setupOp(op: unknown, args: unknown): Promise<SetupResult> {
	const session = setupSession;
	if (session === null) return { kind: 'error', message: 'Setup is not running.' };
	const fields = typeof args === 'object' && args !== null ? (args as Record<string, unknown>) : {};
	try {
		switch (op) {
			case 'view':
			case 'back': {
				// The page owns the step number: 'back' steps one back, 'view' with a step names it directly
				// (Continue calls setup('view', {step: next})). A step name outside the fixed order is ignored,
				// so the page cannot jump to a step that does not exist.
				if (op === 'back') session.step = STEP_ORDER[Math.max(0, STEP_ORDER.indexOf(session.step) - 1)];
				else if (typeof fields.step === 'string' && STEP_ORDER.includes(fields.step as SetupStep)) session.step = fields.step as SetupStep;
				const { config, globals } = await readGlobalConfig();
				session.globals = globals;
				return { kind: 'view', view: setupViewFor(session.step, config, globals, session) };
			}
			case 'accounts':
			case 'addAccount':
				return await addAccount(session, fields);
			case 'mode':
				return await applyMode(session, fields);
			case 'decline':
				return await applyModeChoice(session, 'classify-only');
			case 'detect':
			case 'modelRetry':
				return await runDetect(session, fields.url);
			case 'pull':
				return await startPull(session, fields);
			case 'save':
				return await saveModel(session, fields);
			case 'open':
				// "install Ollama": one fixed constant from setup.ts, never an address the page typed
				// (that field feeds `model detect --url`, and only that).
				void shell.openExternal(OLLAMA_INSTALL_URL);
				return { kind: 'view', view: modelStepView(session) };
			case 'cancelDownload':
				session.handle?.cancel();
				return { kind: 'view', view: modelStepView(session) };
			case 'digest':
				return await setDigest(session, fields);
			case 'settings':
				return await setNotifications(session, fields);
			case 'start':
				return await finishSetup(session);
		}
	} catch (failed) {
		return { kind: 'error', message: firstLine(failed) };
	}
	return { kind: 'error', message: 'Not a known setup action.' };
}

// The notification poll, in the main process so a hidden window or popup still gets notifications
// (their own polls stop with the surface). Every 30 s while a switch is on and setup finished;
// notifier.ts decides whether a change is worth a sentence, and the first read is only a baseline.
const NOTIFY_POLL_MS = 30_000;
function notifyTick(): void {
	if (settings.setupDone && (notifySwitches.notifyReply || notifySwitches.notifyOffline)) {
		void engine.status().then((result) => {
			if (result.kind === 'status') notifier.poll(result.report);
		});
	}
	setTimeout(notifyTick, NOTIFY_POLL_MS).unref();
}

// The clock the popup's strings are built with: the user's locale and zone, pinned once per
// read rather than guessed by the page (see popup-model.ts).
const clockOptions = () => ({
	locale: app.getLocale(),
	timeZone: Intl.DateTimeFormat().resolvedOptions().timeZone,
});

// Whether the live engine is the one this app spawned. A fresh `status` that reports `running`
// while this is false means something else started it (a launchd agent or a terminal). The engine's
// own lock would refuse a second watcher anyway; this guard just keeps the popup from offering one.
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
			void pauseEngine();
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
	if (!external) {
		// A Start this session clears the pause marker: the engine is live again, so "paused" is gone.
		pausedAt = null;
		engine.start();
	}
}

// The Stop/Press-pause action, shared by the tray menu, the window button and the popup button. It
// records the moment (memory only) so the popup can read "paused at HH:MM", then stops the engine
// exactly as before (one SIGTERM then SIGKILL). Only Stop goes through here; the quit path (before-quit)
// stops without marking a pause, because the app is ending, not pausing.
async function pauseEngine(): Promise<void> {
	pausedAt = Math.floor(Date.now() / 1000);
	await engine.stop();
}

// The `paused` argument for both `buildPopupView` calls: only this app's own `stopped` state is a pause
// (a crash is `failed`, a live engine is `running`, and `starting`/`stopping` are in-between). `at` is
// the moment we saw the Stop, or null after a restart, which the view model turns into the time-free
// sentence. `buildPopupView` also ignores it while the report says the engine is running.
const pausedOption = (): { at: number | null } | null =>
	engine.getState().state === 'stopped' ? { at: pausedAt } : null;

// One accounts-screen action: validate, confirm if it is consequential, run, then restart the engine
// the app owns so a change takes effect (the engine reads its config only at start).
async function editAccount(op: unknown, fields: unknown): Promise<EditResult> {
	const built = buildEditRequests(op, fields);
	if ('error' in built) return { ok: false, error: built.error };
	for (const request of built.requests) {
		if (request.confirm && !(await confirm(request.confirm))) return { ok: false, error: 'Cancelled.' };
	}
	let result: EditResult = { ok: true };
	for (const request of built.requests) {
		const raw = await engine.runConfig(request.args, request.stdin, request.slow ? CONFIG_TEST_TIMEOUT_MS : undefined);
		result = toEditResult(op as AccountOp, raw);
		if (!result.ok) return result;
	}
	if (result.restart === undefined) return result;
	if (appOwnsEngine()) {
		await engine.stop();
		engine.start();
		return { ...result, restart: 'done' };
	}
	const status = await engine.status();
	return { ...result, restart: status.kind === 'status' && status.report.engine.running ? 'external' : 'none' };
}

// The native confirmation for going live or removing an account; "cancel" is the default button.
async function confirm(kind: 'live' | 'remove'): Promise<boolean> {
	const live = kind === 'live';
	const options = {
		type: 'warning' as const,
		buttons: [live ? 'go live' : 'remove', 'cancel'],
		defaultId: 1,
		cancelId: 1,
		message: live ? 'Let triage change this account on the server?' : 'Remove this account?',
		detail: live
			? 'Triage will set flag colours on the server. It never moves or deletes mail.'
			: 'The account and its saved password are removed. Mail on the server is not touched.',
	};
	const { response } = currentWindow ? await dialog.showMessageBox(currentWindow, options) : await dialog.showMessageBox(options);
	return response === 0;
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
	void engine.stop().then(() => {
		app.quit();
		// Safety net: if Electron swallows the re-entrant quit (seen with SIGTERM), still exit 0.
		setTimeout(() => app.exit(0), 3000).unref();
	});
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
	// The live notification switches start from the saved settings, not from a fixed true: an opt-out
	// (or a silent skip, both saved false) must survive a relaunch instead of snapping back on.
	notifySwitches.notifyReply = settings.notifyReply;
	notifySwitches.notifyOffline = settings.notifyOffline;
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
	if (!app.isPackaged) app.dock?.setIcon(APP_ICON);
	// Without this the About panel of a dev run shows Electron's own name and version.
	app.setAboutPanelOptions({ applicationName: app.name, applicationVersion: app.getVersion(), version: '' });
	app.dock?.hide(); // menu-bar companion: no dock icon until a window is shown
	app.on('window-all-closed', () => ctl.handleWindowAllClosed());

	// On by default ("closing the window leaves triage running"); performStart skips it when an
	// engine the app did not spawn is already live. LTM_AUTOSTART=0 is the dev escape hatch;
	// =1 starts without that check (the smoke test's fake engine reports a running one).
	if (process.env.LTM_AUTOSTART === '1') engine.start();
	else if (process.env.LTM_AUTOSTART !== '0') void performStart();

	ipcMain.handle(CHANNEL.getState, (event) => (fromUs(event.senderFrame) ? engine.getState() : null));
	ipcMain.handle(CHANNEL.getSettings, (event) => (fromUs(event.senderFrame) ? settingsPayload() : null));
	// Validated at the trust boundary: our own page, a known name, a value from the allow-list.
	ipcMain.handle(CHANNEL.setSetting, (event, name: unknown, value: unknown) => {
		if (!fromUs(event.senderFrame) || !isAllowedSetting(name, value)) return null;
		if (name === 'openAtLogin') app.setLoginItemSettings({ openAtLogin: value as boolean });
		else if (name === 'notifyReply' || name === 'notifyOffline') {
			// The two notification switches, now rows on the settings screen. The page may set them
			// (unlike `setupDone`, which stays main-only); the live poll picks the change up on its
			// next read of the same switch object.
			if (name === 'notifyReply') {
				settings = { ...settings, notifyReply: value as boolean };
				notifySwitches.notifyReply = value as boolean;
			} else {
				settings = { ...settings, notifyOffline: value as boolean };
				notifySwitches.notifyOffline = value as boolean;
			}
			persistSettings();
		} else {
			settings = { ...settings, appearance: value as Settings['appearance'] };
			nativeTheme.themeSource = settings.appearance;
			persistSettings();
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
				paused: pausedOption(),
			}))
			: { kind: 'error', message: result.message };
		// Ask for a re-measure, deferred with `setImmediate` so this handler's return value reaches
		// the page first; the two-frame wait inside POPUP_MEASURE then covers the paint. The page
		// polls every 5 s while visible, so a missed measurement self-corrects within one poll.
		if (fromPopup) setImmediate(() => void fitPopup());
		return payload;
	});
	// The accounts screen. Both come from the window page only: the popup must not read or write the
	// config. `getConfig` is the engine's `config show`, trimmed; `editAccount` validates every field
	// (account-edit.ts), asks for a native confirmation before going live or removing an account,
	// runs the engine's own `config account ...` command and restarts the engine it owns.
	ipcMain.handle(CHANNEL.getConfig, async (event) => {
		if (!fromWindow(event.senderFrame)) return null;
		try {
			return parseConfigShow(await engine.runConfig(['show', '--json']));
		} catch (failed) {
			return parseConfigShow({ ok: false, error: firstLine(failed) }); // no engine binary, say
		}
	});
	ipcMain.handle(CHANNEL.editAccount, async (event, op: unknown, fields: unknown): Promise<EditResult | null> => {
		if (!fromWindow(event.senderFrame)) return null;
		try {
			return await editAccount(op as AccountOp, fields);
		} catch (failed) {
			return { ok: false, error: firstLine(failed) };
		}
	});
	ipcMain.handle(CHANNEL.command, async (event, command: EngineCommand) => {
		if (!fromUs(event.senderFrame)) {
			return { command, view: engine.getState(), error: 'sender not allowed' } satisfies CommandResult;
		}
		if (command === 'start') await performStart();
		else if (command === 'stop') await pauseEngine();
		else if (command === 'openWindow') {
			popupCtl.close();
			ctl.open();
		} else if (command === 'openSettings') {
			popupCtl.close();
			runAction('openSettings');
		} else if (command === 'openMail') {
			// One of the popup's three count buttons: open Apple Mail itself (no folder). The argv is
			// fixed in open-mail.ts and this command carries no argument, so nothing from the page is
			// ever interpolated; the handler already refused any sender that is not one of our two pages.
			popupCtl.close();
			void openMail();
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

	// First-run onboarding, the window page only: the popup answers idle and can never drive setup.
	// One channel, closed op names (ipc.ts SetupOp); every case answers with the step's view, so the
	// page never needs a second round-trip to repaint.
	ipcMain.handle(CHANNEL.setup, async (event, op: unknown, args: unknown): Promise<SetupResult> => {
		if (!fromWindow(event.senderFrame)) return { kind: 'idle' };
		return await setupOp(op, args);
	});

	// The first-run decision, in one config read (setup.ts decideSetup): a first run opens the window
	// itself with setup showing (the app normally starts hidden); an install that already has accounts
	// records setupDone silently and opens nothing; a broken or absent config file keeps the normal
	// screen, which is where an engine error belongs.
	void readGlobalConfig().then(({ config, globals }) => {
		const decision = decideSetup(settings.setupDone, config);
		if (decision === 'setup') {
			setupSession = {
				step: 'accounts',
				modeChoice: 'classify-and-sort',
				globals,
				detect: null,
				modelSaved: false,
				checkAnswer: null,
				pullState: 'idle',
				pullModel: '',
				pull: null,
				pullError: null,
				handle: null,
			};
			ctl.open();
		} else if (decision === 'silent') {
			// An install that already has accounts skips onboarding: setup is done and both first-run
			// switches are persisted false (the board's decision), and the live switches drop with them
			// so this session stays silent too.
			settings = skippedInstallSettings(settings);
			notifySwitches.notifyReply = false;
			notifySwitches.notifyOffline = false;
			persistSettings();
		}
	});
	// The notification poll starts here, not with a surface: a closed window still gets its
	// notifications (docs/DESKTOP.md: the popup and window polls stop with their surface).
	notifyTick();

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
		process.exit(1); // typed `never`; never throw here, an uncaught error pops a modal dialog
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
		// 2. The popup's data path in all three states — normal, calm and paused — so the smoke exercises
		//    the same three the screenshots do. `status` is parsed and turned into ready-to-print strings
		//    without rendering a window: that proves the engine round-trip and the view model, not the pixels.
		//    The desktop always runs `status --json`, so the fake engine is told which state to report through
		//    LTM_FAKE_STATUS (the child inherits the parent environment; see EngineController's spawn).
		const expect = (view: PopupView, label: string, needles: string[]): void => {
			// The counts strip is the whole point of this surface: exactly three cells (needs reply, fyi,
			// done), at least one account row, and a non-empty note line — in every state.
			if (view.strip.length !== 3) fail(`${label}: strip is not three cells: ${view.strip.length}`);
			if (view.accounts.length === 0 && view.summary !== 'No accounts configured.') fail(`${label}: no rows`);
			if (view.note === '') fail(`${label}: empty note`);
			const line = `${view.strip.map((cell) => `${cell.label} ${cell.count}`).join(' · ')} · paused=${view.paused} · ${view.note}`;
			for (const needle of needles) if (!line.includes(needle)) fail(`${label}: missing "${needle}" in ${line}`);
			console.error(`SMOKE POPUP OK: ${label}: ${line}`);
		};
		const asView = (result: StatusResult, label: string): PopupView => {
			if (result.kind !== 'status') return fail(`${label}: status --json did not produce a report`);
			return buildPopupView(result.report, { ...clockOptions(), paused: pausedOption() });
		};
		void engine
			.status() // normal: needs reply 4, engine running, not paused
			.then((normal) => {
				expect(asView(normal, 'normal'), 'normal', ['needs reply 4', 'paused=false']);
				process.env.LTM_FAKE_STATUS = 'calm';
				return engine.status(); // calm: needs reply 0 everywhere, engine running, not paused
			})
			.then((calm) => {
				expect(asView(calm, 'calm'), 'calm', ['needs reply 0', 'paused=false']);
				return pauseEngine(); // this app presses pause: pausedAt recorded in memory, engine stops
			})
			.then(() => {
				process.env.LTM_FAKE_STATUS = 'paused';
				return engine.status(); // paused: engine stopped AND this app saw the Stop -> the paused note
			})
			.then((paused) => {
				delete process.env.LTM_FAKE_STATUS; // back to the plain report for any later caller
				expect(asView(paused, 'paused'), 'paused', ['paused=true', 'Paused']);
				// 3. Quit must stop the engine and end the app within the 5s deadline (exit 0).
				app.on('will-quit', () => {
					clearTimeout(deadline);
					console.error('SMOKE OK: window hid, engine ran, popup covered normal/calm/paused, quit sent SIGTERM, exit 0');
				});
				app.quit();
			})
			.catch((error: unknown) => fail(`status read threw: ${String(error)}`));
	};
	waitForRunning(0);
}
