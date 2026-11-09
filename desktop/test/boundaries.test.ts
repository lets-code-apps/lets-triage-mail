import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { CHANNEL } from '../src/ipc.ts';

// Mirrors the Rust boundary check in src/imap/session.rs (`only_session_rs_speaks_imap`): scan
// WHOLE files, assemble the forbidden words so the intent is explicit, and fail with the offending
// lines. This is the executable statement of the desktop/engine boundary: the desktop makes no
// network calls, never reads config or the DB, and touches no engine library.

const DESKTOP = join(import.meta.dirname, '..');
const SRC = join(DESKTOP, 'src');
const DOCS = join(DESKTOP, '..', 'docs', 'DESKTOP.md');

function walk(dir: string): string[] {
	const out: string[] = [];
	for (const entry of readdirSync(dir)) {
		const path = join(dir, entry);
		if (statSync(path).isDirectory()) out.push(...walk(path));
		else out.push(path);
	}
	return out;
}

// "Never makes a network call": no Node HTTP/TLS/socket/dgram modules, no global fetch, and no
// Electron net/session fetch. Assembled so the list reads as intent.
const FORBIDDEN_NETWORK = [
	['node', ':', 'http'].join(''),
	['node', ':', 'https'].join(''),
	['node', ':', 'net'].join(''),
	['node', ':', 'dgram'].join(''),
	['fetch', '('].join(''),
	['session', '.', 'fetch'].join(''),
	['net', '.', 'fetch'].join(''),
];

// "Never reads config or state, and never re-implements engine parsing."
const FORBIDDEN_ENGINE = [['.', 'toml'].join(''), ['state', '.', 'db'].join('')];

test('package.json pulls in no config, database, IMAP or HTTP-client library', () => {
	const pkg = JSON.parse(readFileSync(join(DESKTOP, 'package.json'), 'utf8'));
	const deps = Object.keys({ ...pkg.dependencies, ...pkg.devDependencies }) as string[];
	const forbidden = ['toml', 'sqlite', 'imap', 'undici', 'axios', 'node-fetch', 'got', 'superagent', 'http'];
	for (const name of deps) {
		for (const word of forbidden) {
			assert.ok(!name.includes(word), `desktop must not depend on ${name} (matches ${word})`);
		}
	}
	// It is allowed to depend on Electron and the TypeScript toolchain only.
});

test('no source file makes a network call or reads config/state', () => {
	const files = walk(SRC).filter((f) => f.endsWith('.ts'));
	const violations: string[] = [];
	for (const file of files) {
		const text = readFileSync(file, 'utf8');
		for (const token of [...FORBIDDEN_NETWORK, ...FORBIDDEN_ENGINE]) {
			if (text.includes(token)) violations.push(`${file}: ${token}`);
		}
	}
	assert.deepEqual(violations, []);
});

test('the only child_process use is engine.ts', () => {
	const files = walk(SRC).filter((f) => f.endsWith('.ts'));
	const token = ['child', '_', 'process'].join('');
	const offenders = files.filter((f) => readFileSync(f, 'utf8').includes(token));
	assert.deepEqual(offenders.map((f) => f.slice(SRC.length + 1)), ['engine.ts']);
});

test('DESKTOP.md keeps its Implemented / Mockup only / Future sections', () => {
	const doc = readFileSync(DOCS, 'utf8');
	for (const heading of ['## Implemented', '## Mockup only', '## Future']) {
		assert.ok(doc.includes(heading), `DESKTOP.md is missing the "${heading}" section`);
	}
});

// The preload is CommonJS (sandbox), so it cannot import CHANNEL from ipc.ts. This closes the seam
// the type system cannot: every channel name ipc.ts declares must appear in the bridge it exposes.
test('the preload bridge exposes exactly the five channels ipc.ts declares', () => {
	const preload = readFileSync(join(SRC, 'preload.cjs'), 'utf8');
	const channels = [CHANNEL.getState, CHANNEL.state, CHANNEL.command, CHANNEL.checkResult, CHANNEL.getStatus];
	for (const channel of channels) {
		assert.ok(preload.includes(channel), `preload.cjs must use channel ${channel}`);
	}
	// Five and only five: nothing here reaches Node, the network or the filesystem. The one
	// `require` allowed is the electron module itself.
	const requires = preload.match(/require\([^)]*\)/g) ?? [];
	for (const call of requires) {
		assert.equal(call, "require('electron')", `preload.cjs may only require electron, saw ${call}`);
	}
	for (const forbidden of ['node:', 'readFile', 'fetch', 'child_process', 'process.']) {
		assert.ok(!preload.includes(forbidden), `preload.cjs must not touch ${forbidden}`);
	}
});

// The popup is a second page, so it gets the same lockdown as the window: one exact CSP, no
// markup sink, and the same two-page allow-list in main. These assertions are the executable
// version of docs/DESKTOP.md's "Window and tray security stance".
test('the popup page is as locked down as the window page', () => {
	const popup = readFileSync(join(DESKTOP, 'renderer', 'popup.html'), 'utf8');
	const CSP = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'" />`;
	assert.ok(popup.includes(CSP), 'popup.html must carry the exact same CSP as index.html');
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	// Engine strings are hostile data: text only, never an HTML sink.
	assert.equal(/\.innerHTML\s*=/.test(script), false, 'popup.js must not assign innerHTML');
	assert.ok(script.includes('textContent'), 'popup.js must render with textContent');
});

test('main trusts only the two pages it loads, and answers getStatus behind that guard', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	// Two known files, spelled out; anything else is refused.
	assert.ok(main.includes('url === RENDERER_URL') && main.includes('url === POPUP_URL'), 'fromUs must name exactly two pages');
	assert.ok(/senderFrame\?\.url/.test(main), 'the guard must read the sender frame');
	// A stranger gets `idle`; the popup is answered while the popup is visible, the window page
	// while the window is visible (it no longer gates only on the popup).
	assert.ok(main.includes(`if (!fromUs(event.senderFrame)) return IDLE_PAYLOAD`), 'getStatus must be guarded');
	assert.ok(main.includes('fromPopup ? popupCtl.isVisible() : currentWindow?.isVisible()'), 'getStatus must answer the window page too');
	// The tray glyph is the design's template image, not a placeholder with a text label.
	assert.equal(main.includes("setTitle('LTM')"), false, 'no text label in the menu bar');
	assert.ok(main.includes('menubarTemplate.png'), 'the tray must use the template icon');
	// A popup must never make the app icon appear: the dock belongs to the window controller.
	const popupLines = main.slice(main.indexOf('function makePopupWindow'), main.indexOf('const popupCtl'));
	assert.ok(!popupLines.includes('dock.'), 'the popup must not touch the dock');
});

// A popup must follow its content on an engine error too, not only on a good status: the error
// renderer clears the account rows a tall list left behind (stale bounds -> an empty band), and a
// long first error must grow past the 260 px fallback. So the error payload is built, then measured,
// then returned -- it must never be returned ahead of the re-measure.
test('a getStatus error is built and measured, not returned before the re-measure', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	// Bound to the getStatus handler: from its open to the next handler.
	const start = main.indexOf('CHANNEL.getStatus, async');
	const handler = main.slice(start, main.indexOf('ipcMain.handle(CHANNEL.command', start));
	// The engine error is folded into the reply, never early-returned ahead of the measurement.
	assert.doesNotMatch(
		handler,
		/return\s*\{\s*kind:\s*'error'/,
		'an engine error must not return before fitPopup is scheduled',
	);
	// Exactly one deferred measurement for every non-idle reply, after the payload is built.
	assert.equal((handler.match(/setImmediate\(\(\) => void fitPopup\(\)\)/g) ?? []).length, 1, 'fitPopup must be scheduled once');
	assert.match(handler, /const payload[\s\S]*?setImmediate\(\(\) => void fitPopup\(\)\);[\s\S]*?return payload;/, 'payload built, then measured, then returned');
});

// macOS Spaces: several desktops can share one monitor, and a window that belongs to a desktop the
// user is not looking at drags them back to it when it appears. The claim lives in main (Electron
// only), behind `PopupHandle`, is applied at most once per window because each call briefly hides the
// window and the Dock icon, and it must not lift the popup above a full-screen app on another desktop.
test('the popup claims the current desktop through the handle, once per window, not above fullscreen apps', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	const popup = readFileSync(join(SRC, 'popup.ts'), 'utf8');
	// The tested module decides the order: claim, place, show.
	assert.match(popup, /window\.followCurrentSpace\(\);\s*this\.place\(window\);\s*window\.show\(\);/, 'claim must precede place and show');
	assert.ok(main.includes('setVisibleOnAllWorkspaces(true)'), 'main maps the claim onto Electron');
	assert.ok(main.includes('isVisibleOnAllWorkspaces()'), 'and applies it at most once per window');
	assert.equal(/visibleOnFullScreen:\s*true/.test(main), false, 'never above a full-screen app on another desktop');
});

// The tray advertises "Check setup" as an implemented action, so its click must (a) open a window
// (somewhere to show the result) and (b) push the finished check to the renderer. Both main.ts call
// sites build their payload through the one electron-free checkPayload helper, so the report and the
// error can never be silently dropped.
test('the tray Check result is opened, built and pushed to the renderer', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.ok(main.includes('function deliverCheck'), 'the tray must have a check path');
	assert.ok(/function deliverCheck[^]*?ctl\.open\(\)/.test(main), 'deliverCheck must open the window');
	assert.ok(main.includes('checkPayload('), 'the check result must be built by checkPayload');
	assert.ok(main.includes(`send(CHANNEL.checkResult`), 'the result must be pushed to the renderer');
});

// The popup styles its rows with `.accounts` / `.accounts > li`; a selector the markup does not
// expose is silent breakage (an unstyled browser list). This pins markup and selector together.
test('the account list exposes the class its CSS targets, and carries the date/time line', () => {
	const html = readFileSync(join(DESKTOP, 'renderer', 'popup.html'), 'utf8');
	const css = readFileSync(join(DESKTOP, 'renderer', 'popup.css'), 'utf8');
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	const ul = /<ul\b[^>]*>/.exec(html);
	assert.ok(ul, 'popup.html must contain the account list');
	assert.match(ul[0], /id="accounts"/);
	assert.match(ul[0], /class="accounts"/, 'the list needs the class popup.css styles');
	assert.match(css, /\.accounts\s*>\s*li/, 'the stylesheet must target that class');
	// The required date-and-time line above the counts: present in markup, filled with textContent.
	assert.match(html, /id="generated"/, 'the popup needs a generated date/time element');
	assert.match(script, /generatedEl\.textContent/);
});

// There is no single-instance lock yet, so a Start must not double-watch a mailbox. The guard reads
// a fresh `status`, the view model names the external engine, and the page only reflects the choice.
test('the popup cannot start a second engine while one runs outside the app', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.ok(/function performStart/.test(main), 'Start must go through a guard, not engine.start()');
	assert.ok(/performStart[^]*?engine\.status\(\)/.test(main), 'the guard reads a fresh status');
	const model = readFileSync(join(SRC, 'popup-model.ts'), 'utf8');
	assert.ok(model.includes("'Running outside the app.'"), 'the view model names the external engine');
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	assert.match(script, /startBtn\.disabled = Boolean\(view\.startDisabled\)/);
});

// The popup has no room for a report and no onCheckResult sink of its own, so its "Check setup"
// must reuse the tray path (open the window + push the result) rather than discard a returned one.
test('the popup Check setup opens the window instead of discarding its report', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.match(main, /POPUP_URL[\s\S]*?deliverCheck\(\)/, 'a popup check must call deliverCheck');
});

// A window opened while the engine is already running must show the real state and enable the right
// controls at once: the renderer reads getState() on load before subscribing to future transitions.
test('the renderer bootstraps the current state and subscribes to check results', () => {
	const renderer = readFileSync(join(DESKTOP, 'renderer', 'renderer.js'), 'utf8');
	assert.ok(renderer.includes('api.getState()'), 'the renderer must read the current state on load');
	assert.ok(renderer.indexOf('api.getState()') < renderer.indexOf('api.onState('), 'read before subscribe');
	assert.ok(renderer.includes('api.onCheckResult('), 'the renderer must show tray check results');
	// Engine output stays hostile data: state is written with textContent, never an innerHTML assignment.
	assert.ok(renderer.includes('summaryEl.textContent'), 'state must render with textContent');
	assert.equal(/\.innerHTML\s*=/.test(renderer), false); // no markup sink, even though the word may appear in prose
	// Same lockdown as the popup: the exact same CSP in both pages, and the window polls getStatus.
	const csp = (name: string) => /<meta http-equiv="Content-Security-Policy"[^>]*>/.exec(readFileSync(join(DESKTOP, 'renderer', name), 'utf8'))?.[0];
	assert.ok(csp('index.html'), 'index.html must carry a CSP');
	assert.equal(csp('index.html'), csp('popup.html'), 'both pages must carry the identical CSP');
	assert.ok(renderer.includes('api.getStatus()'), 'the window polls getStatus');
});
