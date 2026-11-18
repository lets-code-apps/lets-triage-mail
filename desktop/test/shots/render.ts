// The desktop screenshot + layout harness. It renders the real window page eight times (accounts
// and settings, at 1100px and 360px, in light and dark) with SYNTHETIC data, measures the design
// layout, captures a PNG, and compares that PNG against the committed baseline in
// `desktop/screenshots/`. It is run by the `desktop-shots` CI job (see .github/workflows/ci.yml);
// nothing here ships in the app.
//
// Why the real preload and IPC: the LET-103 shots used a stub bridge. Loading `src/preload.cjs` and
// answering the nine channels with `ipcMain.handle` in the main process exercises the real bridge
// too, and lets the page render exactly as it does in the app. All answers are the committed fixtures
// and literals below: NO real config, NO `~/.config`, NO engine, NO network. Do not trigger a setup
// check in a shot: it prints `new Date()` ("Checked at HH:MM") and the PNG would change every minute.
//
// It imports `src/popup-model.ts` and `src/status-report.ts` (electron-free) to build the status
// payload the same way main does, and the two pure helpers in this folder (`compare.ts`,
// `layout.ts`) for the diff and the layout rules. Electron 45's Node strips the TypeScript types,
// so this file runs as `electron test/shots/render.ts` with no build step.

import { app, BrowserWindow, ipcMain, nativeImage, nativeTheme } from 'electron';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

import { buildPopupView } from '../../src/popup-model.ts';
import { parseStatusReport } from '../../src/status-report.ts';
import { MAX_CHANGED_RATIO, PIXEL_DELTA, compareShots, makeDiffBitmap } from './compare.ts';
import { checkLayout, MEASURE } from './layout.ts';

const HERE = import.meta.dirname;
const DESKTOP = join(HERE, '..', '..');
const RENDERER_HTML = join(DESKTOP, 'renderer', 'index.html');
const PRELOAD = join(DESKTOP, 'src', 'preload.cjs');
const FIXTURE = join(DESKTOP, 'test', 'fixtures', 'status-report.json');
const SHOTS = join(DESKTOP, 'screenshots');
const OUT_ACTUAL = join(DESKTOP, 'shots-out', 'actual');
const OUT_DIFF = join(DESKTOP, 'shots-out', 'diff');

const HEIGHT = 760;
const WIDTHS = [1100, 360];
const SECTIONS = ['accounts', 'settings'];
const THEMES = ['light', 'dark'];
const CAPTURE_MS = 10_000;
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

// Wait until the page has really painted this section, not just until `load` fired. The first state
// rendered in a run is otherwise captured mid-bootstrap: its account rows or settings rows are not
// built yet, so its panes are a different height and a scrollbar appears only in one run -> the shot
// differs run-to-run. The probe names the settled shape for each section; `renderOne` polls it.
const SETTLED: Record<'accounts' | 'settings', string> = {
	accounts: `(()=>{ const rows=document.querySelector('#rows'); const detail=document.querySelector('#detail');
		return rows && rows.children.length>=2 && detail && detail.children.length>=1 && document.querySelector('#settings').hidden===true; })()`,
	settings: `(()=>{ const s=document.querySelector('#settings');
		return s && s.hidden===false && document.querySelectorAll('#settings .srow').length>=5; })()`,
};

// A stuck capturePage or loadFile must fail fast with a message, never hang the whole 10-minute job.
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

// The status payload for the accounts screen, built exactly like main does: parse the fixture, then
// format it. Pinning locale/zone makes the "since HH:MM" text identical on every machine, so the
// accounts PNGs do not depend on the runner's timezone.
function buildStatus(): { kind: 'status'; view: ReturnType<typeof buildPopupView> } {
	const parsed = parseStatusReport(readFileSync(FIXTURE, 'utf8'));
	if ('error' in parsed) throw new Error(`fixture is not a valid status report: ${parsed.error}`);
	return { kind: 'status', view: buildPopupView(parsed.report, { locale: 'en-US', timeZone: 'UTC' }) };
}

// Answer the channels the page invokes on load and via its controls. onState/onCheckResult are
// listeners, not invokes, so they need no handler; and we never push a check result, which would
// paint a wall clock ("Checked at HH:MM") into a shot.
function registerIpc(): void {
	const status = buildStatus();
	ipcMain.handle('ltm.getState', () => ENGINE_VIEW);
	ipcMain.handle('ltm.getStatus', () => status);
	ipcMain.handle('ltm.getConfig', () => CONFIG_SHOW);
	ipcMain.handle('ltm.getSettings', () => SETTINGS);
	ipcMain.handle('ltm.setSetting', () => SETTINGS);
	ipcMain.handle('ltm.command', (_event, command: string) => ({ command, view: ENGINE_VIEW }));
	ipcMain.handle('ltm.editAccount', () => null);
}

// Bring one image to CSS pixels. A Retina Mac captures 2x; a CI runner may capture 1x. Resizing to
// (width, HEIGHT) makes the baseline identical on every machine, which is the point of comparing
// bytes here rather than eyeballing a shot.
function normalise(image: Electron.NativeImage, width: number): Electron.NativeImage {
	const size = image.getSize();
	if (size.width === width && size.height === HEIGHT) return image;
	return image.resize({ width, height: HEIGHT, quality: 'best' });
}

const bitmapOf = (image: Electron.NativeImage): { width: number; height: number; data: Uint8Array } => {
	const size = image.getSize();
	return { width: size.width, height: size.height, data: image.toBitmap() };
};

// Poll the page until it has fully painted the requested section, then a short beat for fonts and the
// section transition. Returns once the probe is true (or after ~3 s, so a broken page fails as an
// empty shot rather than hanging).
async function settle(win: BrowserWindow, section: 'accounts' | 'settings'): Promise<void> {
	const probe = SETTLED[section];
	for (let attempt = 0; attempt < 60; attempt += 1) {
		const ready = await withTimeout(win.webContents.executeJavaScript(probe), 3000, `settle ${section}`);
		if (ready) break;
		await sleep(50);
	}
	await sleep(250);
}

async function renderOne(win: BrowserWindow, section: string, width: number, theme: string): Promise<{ name: string; layout: string[]; actual: { width: number; height: number; data: Uint8Array }; png: Buffer }> {
	const name = `${section}-${width}-${theme}`;
	nativeTheme.themeSource = theme as 'light' | 'dark';
	win.setContentSize(width, HEIGHT);
	await win.loadFile(RENDERER_HTML);
	// Switch to the requested section, then wait until the page has really painted it (not just `load`)
	// so rows/settings exist and no pane is scrolling mid-bootstrap before we measure and capture.
	await win.webContents.executeJavaScript(`document.querySelector('[data-section="${section}"]').click(); true;`);
	await win.webContents.executeJavaScript('document.fonts.ready.then(() => true)');
	await settle(win, section as 'accounts' | 'settings');

	const measured = (await win.webContents.executeJavaScript(MEASURE)) as Parameters<typeof checkLayout>[1];
	const layout = checkLayout(section as 'accounts' | 'settings', measured);

	const raw = await withTimeout(win.webContents.capturePage(), CAPTURE_MS, `capturePage ${name}`);
	const image = normalise(raw, width);
	return { name, layout, actual: bitmapOf(image), png: image.toPNG() };
}

async function main(): Promise<number> {
	registerIpc();

	const update = process.argv.includes('--update');
	if (update && process.env.CI) {
		// Never let CI write the baseline: that is out of scope. A baseline is a reviewed commit.
		console.error('--update is refused when CI is set; update the baseline on purpose, in a PR.');
		return 2;
	}

	mkdirSync(OUT_ACTUAL, { recursive: true });
	if (update) mkdirSync(SHOTS, { recursive: true });
	else mkdirSync(OUT_DIFF, { recursive: true });

	// One window, reused for all eight states. A reload re-paints the fresh page at the new size and
	// theme; destroying it and creating the next made the second load fail with ERR_FAILED.
	const win = new BrowserWindow({
		show: false,
		width: WIDTHS[0],
		height: HEIGHT,
		useContentSize: true,
		webPreferences: { preload: PRELOAD, contextIsolation: true, sandbox: true },
	});

	// Warm-up load: the FIRST page load in a run bootstraps slower (fonts, first IPC round-trips), so
	// without this the first captured state is the least-settled one and drifts run-to-run. Discard it.
	nativeTheme.themeSource = 'light';
	await win.loadFile(RENDERER_HTML);
	await win.webContents.executeJavaScript(`document.querySelector('[data-section="accounts"]').click(); true;`);
	await win.webContents.executeJavaScript('document.fonts.ready.then(() => true)');
	await settle(win, 'accounts');

	const rows: { name: string; status: string; px: number; ratio: number }[] = [];
	const violations: string[] = [];
	let failing = false;

	for (const section of SECTIONS) {
		for (const width of WIDTHS) {
			for (const theme of THEMES) {
				const rendered = await renderOne(win, section, width, theme);
				const name = rendered.name;
				for (const violation of rendered.layout) violations.push(`${name}: ${violation}`);
				writeFileSync(join(OUT_ACTUAL, `${name}.png`), rendered.png);

				const baselinePath = join(SHOTS, `${name}.png`);
				if (update) {
					writeFileSync(baselinePath, rendered.png);
					rows.push({ name, status: 'UPDATED', px: 0, ratio: 0 });
					continue;
				}
				if (!existsSync(baselinePath)) {
					rows.push({ name, status: 'MISSING', px: 0, ratio: 0 });
					failing = true;
					continue;
				}
				const baseline = bitmapOf(nativeImage.createFromBuffer(readFileSync(baselinePath)));
				const shot = compareShots(rendered.actual, baseline, { delta: PIXEL_DELTA, maxRatio: MAX_CHANGED_RATIO });
				if (shot.kind === 'size') {
					rows.push({ name, status: `SIZE ${rendered.actual.width}x${rendered.actual.height} vs ${baseline.width}x${baseline.height}`, px: 0, ratio: 0 });
					failing = true;
					continue;
				}
				if (shot.kind === 'changed') {
					rows.push({ name, status: 'CHANGED', px: shot.changed, ratio: shot.ratio });
					failing = true;
					const diff = makeDiffBitmap(baseline.data, rendered.actual.data, baseline.width, baseline.height, PIXEL_DELTA);
					writeFileSync(join(OUT_DIFF, `${name}.png`), nativeImage.createFromBitmap(Buffer.from(diff), { width: baseline.width, height: baseline.height }).toPNG());
					continue;
				}
				rows.push({ name, status: 'OK', px: shot.changed, ratio: shot.ratio });
			}
		}
	}

	// One line per shot is the "list the changed shots" requirement; the summary table then lands on
	// the GitHub run page when the step-summary env var is present.
	for (const row of rows) {
		if (row.status === 'OK') console.log(`OK      ${row.name} (${row.px} px)`);
		else if (row.status === 'CHANGED') console.log(`CHANGED ${row.name}: ${row.px} px (${(row.ratio * 100).toFixed(2)}%)`);
		else if (row.status === 'UPDATED') console.log(`UPDATED ${row.name}`);
		else if (row.status === 'MISSING') console.log(`MISSING ${row.name}`);
		else console.log(`SIZE    ${row.name}: ${row.status}`);
	}
	for (const violation of violations) console.log(`LAYOUT  ${violation}`);

	const ok = !failing && violations.length === 0;
	console.log(`${ok ? 'ALL' : 'FAILED'} ${rows.length} shots, ${violations.length} layout violation(s), delta=${PIXEL_DELTA}, ratio<=${MAX_CHANGED_RATIO}`);

	if (process.env.GITHUB_STEP_SUMMARY) {
		const table = ['', '| shot | status | px | ratio |', '|---|---|---|---|'];
		for (const row of rows) table.push(`| ${row.name} | ${row.status} | ${row.px} | ${(row.ratio * 100).toFixed(3)}% |`);
		if (violations.length > 0) {
			table.push('', '### layout violations', '');
			for (const violation of violations) table.push(`- ${violation}`);
		}
		writeFileSync(process.env.GITHUB_STEP_SUMMARY, table.join('\n') + '\n', { flag: 'a' });
	}

	writeFileSync(join(OUT_ACTUAL, '..', 'report.json'), JSON.stringify({ ok, rows, violations }, null, 2));
	return ok ? 0 : 1;
}

// Electron's real entry point: when the app is ready, run the pass and quit with its status code.
app.whenReady().then(async () => {
	const code = await main();
	app.exit(code);
});
