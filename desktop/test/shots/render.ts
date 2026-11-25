// The desktop screenshot + layout harness. It renders the real window page eight times (accounts
// and settings, at 1100px and 360px, in light and dark) AND the real popup page six times (normal,
// calm and paused, in light and dark) with SYNTHETIC data, measures the design layout, captures a
// PNG, and compares that PNG against the committed baseline in `desktop/screenshots/` — fourteen
// shots in all. It is run by the `desktop-shots` CI job (see .github/workflows/ci.yml);
// nothing here ships in the app.
//
// Why the real preload and IPC: the LET-103 shots used a stub bridge. Loading `src/preload.cjs` and
// answering the nine channels with `ipcMain.handle` in the main process exercises the real bridge
// too, and lets the page render exactly as it does in the app. All answers are the committed fixtures
// and literals below: NO real config, NO `~/.config`, NO engine, NO network. Do not trigger a setup
// check in a shot: it prints `new Date()` ("Checked at HH:MM") and the PNG would change every minute.
//
// It imports `src/popup-model.ts` and `src/status-report.ts` (electron-free) to build the status
// payload the same way main does, and the three pure helpers in this folder (`compare.ts`, `layout.ts`,
// `report.ts`) for the diff, the layout rules and the pass/fail report. Electron 45's Node strips the
// TypeScript types, so this file runs as `electron test/shots/render.ts` with no build step.
//
// Failure handling: every page load and capture is time-bounded, a per-state failure is recorded as a
// RENDER FAILED row, and any error is folded into `report.ts`'s report so a rejected render still writes
// `report.json` and exits non-zero instead of escaping and skipping the write + exit (review round 1,
// finding 1). `app.exit` always runs.

import { app, BrowserWindow, ipcMain, nativeImage, nativeTheme } from 'electron';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { buildPopupView } from '../../src/popup-model.ts';
import { parseStatusReport } from '../../src/status-report.ts';
import { MAX_CHANGED_RATIO, PIXEL_DELTA, compareShots, makeDiffBitmap } from './compare.ts';
import { checkLayout, checkPopupLayout, MEASURE, MEASURE_POPUP } from './layout.ts';
import { finalize, exitCodeFor } from './report.ts';
import type { Report, ShotRow } from './report.ts';

const HERE = import.meta.dirname;
const DESKTOP = join(HERE, '..', '..');
const RENDERER_HTML = join(DESKTOP, 'renderer', 'index.html');
const POPUP_HTML = join(DESKTOP, 'renderer', 'popup.html');
const PRELOAD = join(DESKTOP, 'src', 'preload.cjs');
const FIXTURE = join(DESKTOP, 'test', 'fixtures', 'status-report.json');
const CALM_FIXTURE = join(DESKTOP, 'test', 'fixtures', 'status-report-calm.json');
const SHOTS = join(DESKTOP, 'screenshots');
const OUT_ACTUAL = join(DESKTOP, 'shots-out', 'actual');
const OUT_DIFF = join(DESKTOP, 'shots-out', 'diff');
const REPORT_JSON = join(DESKTOP, 'shots-out', 'report.json');

const HEIGHT = 760;
const WIDTHS = [1100, 360];
const SECTIONS = ['accounts', 'settings'] as const;
const THEMES = ['light', 'dark'];
// The popup surface: its own page, a portrait content box, and the three states the board asked to
// prove (normal, calm, paused), each in light and dark. Baselines are popup-<state>-<theme>.png.
const POPUP_WIDTH = 360;
const POPUP_HEIGHT = 640;
const POPUP_STATES = ['normal', 'calm', 'paused'] as const;
const LOAD_MS = 15_000; // a page load that never resolves is a failure, not a hang
const CAPTURE_MS = 10_000;
const SCRIPT_MS = 5_000; // in-page click / fonts.ready / probe
// The LET-103 file names, so the committed baseline matches the shots already reviewed in history.
// accounts-1100-light.png ... settings-360-dark.png (8 files).

// Every piece of data a shot shows. All synthetic and example-only: the account names are "Example
// Work/Home", the only host is imap.example.com, the only user is *@example.com, and the only path
// is /example/config.toml. No machine path, user name or real address can reach a PNG.
const ENGINE_VIEW = {
	state: 'running',
	version: 'lets-triage-mail 0.1.0',
	exitCode: null,
	signal: null,
	failureTail: [],
	restartInSeconds: null,
} as const;

const CONFIG_SHOW = {
	ok: true,
	path: '/example/config.toml',
	accounts: [
		{ name: 'Example Work', username: 'work@example.com', host: 'imap.example.com', port: 993, tls: 'tls', mode: 'live' },
		{ name: 'Example Home', username: 'home@example.com', host: 'imap.example.com', port: 993, tls: 'starttls', mode: 'dry-run' },
	],
} as const;

const SETTINGS = {
	appearance: 'system',
	openAtLogin: false,
	log: ['engine: watching example.com'],
	section: null,
} as const;

const sleep = (ms: number): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

// The message text for a thrown value (Error has stack; other throws may not).
const asText = (thrown: unknown): string => {
	if (thrown instanceof Error) return thrown.stack ?? thrown.message;
	return String(thrown);
};

// A stuck loadFile, executeJavaScript or capturePage must fail fast with a message, never hang the
// whole 10-minute job. Used for every page load and every in-page call.
async function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
	let timer: ReturnType<typeof setTimeout> | undefined;
	try {
		return await Promise.race([promise, new Promise<never>((_resolve, reject) => {
			timer = setTimeout(() => reject(new Error(`${label} timed out after ${ms} ms`)), ms);
		})]);
	} finally {
		if (timer !== undefined) clearTimeout(timer);
	}
}

// Wait until the page has really painted this section, not just until `load` fired. The first state
// rendered in a run is otherwise captured mid-bootstrap: its account rows or settings rows are not
// built yet, so its panes are a different height and a scrollbar appears only in one run -> the shot
// differs run-to-run. The probe names the settled shape for each section; `settle` polls it.
const SETTLED: Record<'accounts' | 'settings', string> = {
	accounts: `(()=>{ const rows=document.querySelector('#rows'); const detail=document.querySelector('#detail');
		return rows && rows.children.length>=2 && detail && detail.children.length>=1 && document.querySelector('#settings').hidden===true; })()`,
	settings: `(()=>{ const s=document.querySelector('#settings');
		return s && s.hidden===false && document.querySelectorAll('#settings .srow').length>=5; })()`,
};

// The popup is settled when the strip has its three cells painted and the note line has text. All three
// states fill the note (normal "… done since …", calm "Nothing needs a reply…", paused "Paused…"), so
// this probe holds for each; it never reads a wall clock.
const POPUP_SETTLED = `(()=>{ const cells=document.querySelectorAll('#strip .cell'); const note=document.querySelector('#note');
	return cells.length===3 && note && note.textContent.trim()!==''; })()`;

// The status payload for the window's accounts screen, built exactly like main does: parse a fixture,
// then format it. Pinning locale/zone makes the "since HH:MM" text identical on every machine, so the
// PNGs do not depend on the runner's timezone.
type StatusAnswer = { kind: 'status'; view: ReturnType<typeof buildPopupView> } | { kind: 'idle' };
type EngineAnswer = { state: string; version: string; exitCode: number | null; signal: string | null; failureTail: readonly string[]; restartInSeconds: number | null };

function buildStatus(): StatusAnswer {
	const parsed = parseStatusReport(readFileSync(FIXTURE, 'utf8'));
	if ('error' in parsed) throw new Error(`fixture is not a valid status report: ${parsed.error}`);
	return { kind: 'status', view: buildPopupView(parsed.report, { locale: 'en-US', timeZone: 'UTC' }) };
}

// The popup's three states, built the way main builds a reply. normal and calm read a fixture with the
// engine running (only the counts differ); paused reads the normal fixture with the engine not running
// plus the app's in-memory paused marker (a pinned instant, so the "paused at HH:MM" note is deterministic
// under the UTC pin).
function buildPopupStatus(state: 'normal' | 'calm' | 'paused'): StatusAnswer {
	if (state === 'calm') {
		const parsed = parseStatusReport(readFileSync(CALM_FIXTURE, 'utf8'));
		if ('error' in parsed) throw new Error(`calm fixture is not a valid status report: ${parsed.error}`);
		return { kind: 'status', view: buildPopupView(parsed.report, { locale: 'en-US', timeZone: 'UTC' }) };
	}
	const parsed = parseStatusReport(readFileSync(FIXTURE, 'utf8'));
	if ('error' in parsed) throw new Error(`fixture is not a valid status report: ${parsed.error}`);
	if (state === 'paused') {
		parsed.report.engine.running = false;
		parsed.report.engine.started_at = null;
		parsed.report.engine.heartbeat_at = null;
		return { kind: 'status', view: buildPopupView(parsed.report, { locale: 'en-US', timeZone: 'UTC', paused: { at: 1794003300 } }) };
	}
	return { kind: 'status', view: buildPopupView(parsed.report, { locale: 'en-US', timeZone: 'UTC' }) };
}

// The engine view the popup reads via getState: running for normal/calm, stopped for paused (so the
// footer button reads "resume triage", not "start triage"). Only the state field changes.
function popupEngineView(state: 'normal' | 'calm' | 'paused'): EngineAnswer {
	return state === 'paused' ? { ...ENGINE_VIEW, state: 'stopped' } : ENGINE_VIEW;
}

// What getStatus and getState answer right now. Swapped per shot: the popup's six states differ only in
// these two answers, so one handler pair covers all fourteen shots. The window pass answers the normal
// status; the popup pass swaps them before each popup load.
let SHOT_STATUS: StatusAnswer = { kind: 'idle' };
let SHOT_STATE: EngineAnswer = ENGINE_VIEW;

// Answer the channels the page invokes on load and via its controls. onState/onCheckResult are
// listeners, not invokes, so they need no handler; and we never push a check result, which would
// paint a wall clock ("Checked at HH:MM") into a shot.
function registerIpc(): void {
	ipcMain.handle('ltm.getState', () => SHOT_STATE);
	ipcMain.handle('ltm.getStatus', () => SHOT_STATUS);
	ipcMain.handle('ltm.getConfig', () => CONFIG_SHOW);
	ipcMain.handle('ltm.getSettings', () => SETTINGS);
	ipcMain.handle('ltm.setSetting', () => SETTINGS);
	ipcMain.handle('ltm.command', (_event, command: string) => ({ command, view: SHOT_STATE }));
	ipcMain.handle('ltm.editAccount', () => null);
}

// Bring one image to CSS pixels. A Retina Mac captures 2x; a CI runner may capture 1x. Resizing to
// (width, height) makes the baseline identical on every machine, which is the point of comparing bytes
// here rather than eyeballing a shot. Height is 760 for the window and 640 for the popup.
function normalise(image: Electron.NativeImage, width: number, height: number = HEIGHT): Electron.NativeImage {
	const size = image.getSize();
	if (size.width === width && size.height === height) return image;
	return image.resize({ width, height, quality: 'best' });
}

const bitmapOf = (image: Electron.NativeImage): { width: number; height: number; data: Uint8Array } => {
	const size = image.getSize();
	return { width: size.width, height: size.height, data: image.toBitmap() };
};

// Load the window page and switch to a section, then wait until it has painted, so rows/settings exist
// and no pane is scrolling mid-bootstrap before we measure and capture. Every step is time-bounded.
async function loadAndSettle(win: BrowserWindow, section: 'accounts' | 'settings', label: string): Promise<void> {
	await withTimeout(win.loadFile(RENDERER_HTML), LOAD_MS, `loadFile ${label}`);
	await withTimeout(win.webContents.executeJavaScript(`document.querySelector('[data-section="${section}"]').click(); true;`), SCRIPT_MS, `click ${label}`);
	await withTimeout(win.webContents.executeJavaScript('document.fonts.ready.then(() => true)'), SCRIPT_MS, `fonts ${label}`);
	await settle(win, section);
}

// Poll the page until it has fully painted the requested section, then a short beat for fonts and the
// section transition. Each probe call is time-bounded so a broken page fails rather than hangs.
async function settle(win: BrowserWindow, section: 'accounts' | 'settings'): Promise<void> {
	const probe = SETTLED[section];
	for (let attempt = 0; attempt < 60; attempt += 1) {
		const ready = await withTimeout(win.webContents.executeJavaScript(probe), SCRIPT_MS, `settle ${section}`);
		if (ready) break;
		await sleep(50);
	}
	await sleep(250);
}

// Render one state end-to-end: load, measure the layout, capture a CSS-pixel PNG. Throws (bounded) if
// any step fails; `main` turns that into a RENDER FAILED row rather than aborting the whole pass.
async function renderOne(win: BrowserWindow, section: 'accounts' | 'settings', width: number, theme: string): Promise<{ name: string; layout: string[]; actual: { width: number; height: number; data: Uint8Array }; png: Buffer }> {
	const name = `${section}-${width}-${theme}`;
	nativeTheme.themeSource = theme as 'light' | 'dark';
	win.setContentSize(width, HEIGHT);
	await loadAndSettle(win, section, name);

	const measured = (await withTimeout(win.webContents.executeJavaScript(MEASURE), SCRIPT_MS, `measure ${name}`)) as Parameters<typeof checkLayout>[1];
	const layout = checkLayout(section, measured);

	const raw = await withTimeout(win.webContents.capturePage(), CAPTURE_MS, `capturePage ${name}`);
	const image = normalise(raw, width);
	return { name, layout, actual: bitmapOf(image), png: image.toPNG() };
}

// Write the report (report.json) and print the human-readable pass. The step summary is appended when the
// GitHub env var is present, including any error and the layout violations, so the run page lists them.
function emitReport(report: Report, update: boolean): void {
	writeFileSync(REPORT_JSON, JSON.stringify(report, null, 2));

	for (const row of report.rows) {
		if (row.status === 'OK') console.log(`OK      ${row.name} (${row.px} px)`);
		else if (row.status === 'CHANGED') console.log(`CHANGED ${row.name}: ${row.px} px (${(row.ratio * 100).toFixed(2)}%)`);
		else if (row.status === 'UPDATED') console.log(`UPDATED ${row.name}`);
		else if (row.status === 'MISSING') console.log(`MISSING ${row.name}`);
		else console.log(`SHOT    ${row.name}: ${row.status}`);
	}
	for (const violation of report.violations) console.log(`LAYOUT  ${violation}`);
	if (report.error) console.log(`ERROR   ${report.error.split('\n')[0]}`);

	const ok = report.ok;
	console.log(`${ok ? 'ALL' : 'FAILED'} ${report.rows.length} shots, ${report.violations.length} layout violation(s), delta=${PIXEL_DELTA}, ratio<=${MAX_CHANGED_RATIO}`);

	if (process.env.GITHUB_STEP_SUMMARY) {
		const table = ['', '| shot | status | px | ratio |', '|---|---|---|---|'];
		for (const row of report.rows) table.push(`| ${row.name} | ${row.status} | ${row.px} | ${(row.ratio * 100).toFixed(3)}% |`);
		if (report.violations.length > 0) {
			table.push('', '### layout violations', '');
			for (const violation of report.violations) table.push(`- ${violation}`);
		}
		if (report.error) table.push('', '### error', '', `\`\`\`\n${report.error}\n\`\`\``);
		writeFileSync(process.env.GITHUB_STEP_SUMMARY, table.join('\n') + '\n', { flag: 'a' });
	}
	void update;
}

type Rendered = { name: string; layout: string[]; actual: { width: number; height: number; data: Uint8Array }; png: Buffer };

// The compare-and-record half, shared by the window and popup passes: write the actual PNG, then either
// update the baseline (--update), flag MISSING/SIZE/CHANGED, or record OK — with a red diff image for
// any changed shot, so a reviewer sees WHERE a shot moved.
async function storeShot(name: string, rendered: Rendered, update: boolean, rows: ShotRow[], violations: string[]): Promise<void> {
	for (const violation of rendered.layout) violations.push(`${name}: ${violation}`);
	writeFileSync(join(OUT_ACTUAL, `${name}.png`), rendered.png);

	const baselinePath = join(SHOTS, `${name}.png`);
	if (update) {
		writeFileSync(baselinePath, rendered.png);
		rows.push({ name, status: 'UPDATED', px: 0, ratio: 0 });
		return;
	}
	if (!existsSync(baselinePath)) {
		rows.push({ name, status: 'MISSING', px: 0, ratio: 0 });
		return;
	}
	const baseline = bitmapOf(nativeImage.createFromBuffer(readFileSync(baselinePath)));
	const shot = compareShots(rendered.actual, baseline, { delta: PIXEL_DELTA, maxRatio: MAX_CHANGED_RATIO });
	if (shot.kind === 'size') {
		rows.push({ name, status: `SIZE ${rendered.actual.width}x${rendered.actual.height} vs ${baseline.width}x${baseline.height}`, px: 0, ratio: 0 });
		return;
	}
	if (shot.kind === 'changed') {
		rows.push({ name, status: 'CHANGED', px: shot.changed, ratio: shot.ratio });
		const diff = makeDiffBitmap(baseline.data, rendered.actual.data, baseline.width, baseline.height, PIXEL_DELTA);
		writeFileSync(join(OUT_DIFF, `${name}.png`), nativeImage.createFromBitmap(Buffer.from(diff), { width: baseline.width, height: baseline.height }).toPNG());
		return;
	}
	rows.push({ name, status: 'OK', px: shot.changed, ratio: shot.ratio });
}

// Run one window state end-to-end, then fold its outcome into the rows. A throw is recorded (RENDER FAILED)
// and captured into `error` so the pass still produces a report; one bad shot never aborts the rest.
async function captureWindow(win: BrowserWindow, section: 'accounts' | 'settings', width: number, theme: string, update: boolean, rows: ShotRow[], violations: string[]): Promise<void> {
	const name = `${section}-${width}-${theme}`;
	let rendered: Rendered;
	try {
		rendered = await renderOne(win, section, width, theme);
	} catch (thrown) {
		rows.push({ name, status: 'RENDER FAILED', px: 0, ratio: 0 });
		throw Object.assign(new Error(`RENDER FAILED ${name}: ${asText(thrown)}`), { alreadyRecorded: true });
	}
	await storeShot(name, rendered, update, rows, violations);
}

// Load the popup page and wait until the strip and note have painted. The popup has no section switch, so
// there is no click; the probe is POPUP_SETTLED (three cells present, note non-empty).
async function loadAndSettlePopup(win: BrowserWindow, label: string): Promise<void> {
	await withTimeout(win.loadFile(POPUP_HTML), LOAD_MS, `loadFile ${label}`);
	await withTimeout(win.webContents.executeJavaScript('document.fonts.ready.then(() => true)'), SCRIPT_MS, `fonts ${label}`);
	for (let attempt = 0; attempt < 60; attempt += 1) {
		const ready = await withTimeout(win.webContents.executeJavaScript(POPUP_SETTLED), SCRIPT_MS, `settle ${label}`);
		if (ready) break;
		await sleep(50);
	}
	await sleep(250);
}

// Render one popup state: swap the getStatus/getState answers this state reads BEFORE loading (the page
// invokes them on load), then measure the strip's layout and capture a CSS-pixel PNG.
async function renderPopupOne(win: BrowserWindow, state: 'normal' | 'calm' | 'paused', theme: string): Promise<Rendered> {
	const name = `popup-${state}-${theme}`;
	nativeTheme.themeSource = theme as 'light' | 'dark';
	win.setContentSize(POPUP_WIDTH, POPUP_HEIGHT);
	SHOT_STATUS = buildPopupStatus(state);
	SHOT_STATE = popupEngineView(state);
	await loadAndSettlePopup(win, name);

	const measured = (await withTimeout(win.webContents.executeJavaScript(MEASURE_POPUP), SCRIPT_MS, `measure ${name}`)) as Parameters<typeof checkPopupLayout>[0];
	const layout = checkPopupLayout(measured);

	const raw = await withTimeout(win.webContents.capturePage(), CAPTURE_MS, `capturePage ${name}`);
	const image = normalise(raw, POPUP_WIDTH, POPUP_HEIGHT);
	return { name, layout, actual: bitmapOf(image), png: image.toPNG() };
}

// Run one popup state, recording failures the same way the window pass does.
async function capturePopup(win: BrowserWindow, state: 'normal' | 'calm' | 'paused', theme: string, update: boolean, rows: ShotRow[], violations: string[]): Promise<void> {
	const name = `popup-${state}-${theme}`;
	let rendered: Rendered;
	try {
		rendered = await renderPopupOne(win, state, theme);
	} catch (thrown) {
		rows.push({ name, status: 'RENDER FAILED', px: 0, ratio: 0 });
		throw Object.assign(new Error(`RENDER FAILED ${name}: ${asText(thrown)}`), { alreadyRecorded: true });
	}
	await storeShot(name, rendered, update, rows, violations);
}

async function main(): Promise<number> {
	const update = process.argv.includes('--update');
	if (update && process.env.CI) {
		// Never let CI write the baseline: that is out of scope. A baseline is a reviewed commit.
		console.error('--update is refused when CI is set; update the baseline on purpose, in a PR.');
		return 2;
	}

	const rows: ShotRow[] = [];
	const violations: string[] = [];
	let error: string | undefined;

	// Every step below is bounded, but this catch is the last line of defence: an unexpected throw still
	// records the error into the report so the job writes report.json and exits non-zero (never hangs,
	// never silently skips the write + exit).
	try {
		registerIpc();
		mkdirSync(OUT_ACTUAL, { recursive: true });
		if (update) mkdirSync(SHOTS, { recursive: true });
		else mkdirSync(OUT_DIFF, { recursive: true });

		// One window, reused for all fourteen states (eight window shots, six popup shots). A reload
		// re-paints the fresh page at the new size and theme; destroying it and creating the next made
		// the second load fail with ERR_FAILED.
		const win = new BrowserWindow({
			show: false,
			width: WIDTHS[0],
			height: HEIGHT,
			useContentSize: true,
			webPreferences: { preload: PRELOAD, contextIsolation: true, sandbox: true },
		});

		// The window answers are the normal status with the engine running; the popup pass swaps them per
		// state inside renderPopupOne. Set them before the warm-up so the first load reads them.
		SHOT_STATUS = buildStatus();
		SHOT_STATE = ENGINE_VIEW;

		// Warm-up load: the FIRST page load in a run bootstraps slower (fonts, first IPC round-trips), so
		// without this the first captured state is the least-settled one and drifts run-to-run. Best-effort.
		try {
			await loadAndSettle(win, 'accounts', 'warm-up');
		} catch (thrown) {
			console.warn(`warm-up load failed (states will report their own): ${asText(thrown).split('\n')[0]}`);
		}

		for (const section of SECTIONS) {
			for (const width of WIDTHS) {
				for (const theme of THEMES) {
					try {
						await captureWindow(win, section, width, theme, update, rows, violations);
					} catch (thrown) {
						// captureWindow already added a RENDER FAILED row; keep the first error text.
						if (error === undefined) error = asText(thrown);
					}
				}
			}
		}

		// The popup pass on the same reused window: three states, light and dark. renderPopupOne swaps the
		// getStatus/getState answers and loads popup.html, so the same nine-channel bridge is exercised.
		for (const state of POPUP_STATES) {
			for (const theme of THEMES) {
				try {
					await capturePopup(win, state, theme, update, rows, violations);
				} catch (thrown) {
					if (error === undefined) error = asText(thrown);
				}
			}
		}
	} catch (thrown) {
		if (error === undefined) error = asText(thrown);
	}

	const report = finalize({ rows, violations, error });
	emitReport(report, update);
	return exitCodeFor(report);
}

// Electron's real entry point: when the app is ready, run the pass and quit with its status code. The
// catch guards the whole chain so `app.exit` always runs even if `whenReady` or `main` somehow throws --
// a stuck CI job is the exact failure finding 1 was about.
app.whenReady()
	.then(async () => {
		let code = 1;
		try {
			code = await main();
		} catch (thrown) {
			console.error(`shots harness failed after main: ${asText(thrown)}`);
			const report = finalize({ rows: [], violations: [], error: asText(thrown) });
			writeFileSync(REPORT_JSON, JSON.stringify(report, null, 2));
			code = exitCodeFor(report);
		}
		app.exit(code);
	})
	.catch((thrown) => {
		console.error(`shots harness could not start: ${asText(thrown)}`);
		app.exit(1);
	});
