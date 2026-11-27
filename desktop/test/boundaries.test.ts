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

test('the only child_process use is engine.ts and open-mail.ts', () => {
	const files = walk(SRC).filter((f) => f.endsWith('.ts'));
	const token = ['child', '_', 'process'].join('');
	const offenders = files.filter((f) => readFileSync(f, 'utf8').includes(token));
	// engine.ts runs the engine child; open-mail.ts runs the fixed `open -a Mail` for the popup's
	// count buttons. Both use a fixed argv with execFile/spawn, never a shell. Nothing else spawns.
	assert.deepEqual(offenders.map((f) => f.slice(SRC.length + 1)), ['engine.ts', 'open-mail.ts']);
});

// open-mail.ts is the one place a popup button reaches the OS: it must run exactly `open -a Mail`,
// with the argv spelled as literals and never through a shell, so nothing from the page is interpreted.
test('open-mail.ts runs a fixed open -a Mail, never a shell', () => {
	const openMail = readFileSync(join(SRC, 'open-mail.ts'), 'utf8');
	for (const token of ["'open'", "'-a'", "'Mail'"]) {
		assert.ok(openMail.includes(token), `open-mail.ts must pass the literal ${token}`);
	}
	assert.equal(/shell:\s*true/.test(openMail), false, 'open-mail.ts must not run a shell');
	// execFile/spawn take an argv; exec would put one string through a shell. No exec call.
	assert.equal(/[^F]exec\(/.test(openMail), false, 'open-mail.ts must use execFile, not exec');
	// openMail is an allowed command but adds no channel and no preload function (the nine stay).
	const ipc = readFileSync(join(SRC, 'ipc.ts'), 'utf8');
	assert.ok(/EngineCommand =[^;]*'openMail'/.test(ipc), 'EngineCommand includes openMail');
});

test('DESKTOP.md keeps its Implemented / Mockup only / Future sections', () => {
	const doc = readFileSync(DOCS, 'utf8');
	for (const heading of ['## Implemented', '## Mockup only', '## Future']) {
		assert.ok(doc.includes(heading), `DESKTOP.md is missing the "${heading}" section`);
	}
});

// The preload is CommonJS (sandbox), so it cannot import CHANNEL from ipc.ts. This closes the seam
// the type system cannot: every channel name ipc.ts declares must appear in the bridge it exposes.
test('the preload bridge exposes exactly the ten channels ipc.ts declares', () => {
	const preload = readFileSync(join(SRC, 'preload.cjs'), 'utf8');
	const channels = Object.values(CHANNEL);
	assert.equal(channels.length, 10, 'ipc.ts declares ten channels');
	for (const channel of channels) {
		assert.ok(preload.includes(channel), `preload.cjs must use channel ${channel}`);
	}
	// Ten and only ten: nothing here reaches Node, the network or the filesystem. The one
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
	const CSP = `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'" />`;
	assert.ok(popup.includes(CSP), 'popup.html must carry the exact same CSP as index.html');
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	// Engine strings are hostile data: text only, never an HTML sink.
	assert.equal(/\.innerHTML\s*=/.test(script), false, 'popup.js must not assign innerHTML');
	assert.ok(script.includes('textContent'), 'popup.js must render with textContent');
});

test('a status that cannot be answered paints its reason on the new note line, never a removed element', () => {
	// The summary box is gone; a rejected `getStatus` used to write into the removed `sublineEl`, which
	// threw and left the reason invisible. The new layout must route both the engine error payload and a
	// rejected round-trip through the note line, and must not reference any element the old box owned.
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	for (const gone of ['sublineEl', 'pillEl', 'countEl']) {
		assert.equal(script.includes(gone), false, `popup.js must not reference the removed ${gone}`);
	}
	assert.ok(/function showError\(/.test(script), 'the popup needs one shared error paint path');
	assert.match(script, /showError\(String\(payload\.message\)\)/, 'an engine error payload is shown');
	assert.match(script, /showError\(`Status unavailable: \$\{String\(failed\)\}`\)/, 'a rejected status request is shown');
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
test('the account list exposes the class its CSS targets, and the strip names its cells as text', () => {
	const html = readFileSync(join(DESKTOP, 'renderer', 'popup.html'), 'utf8');
	const css = readFileSync(join(DESKTOP, 'renderer', 'popup.css'), 'utf8');
	const script = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	const ul = /<ul\b[^>]*>/.exec(html);
	assert.ok(ul, 'popup.html must contain the account list');
	assert.match(ul[0], /id="accounts"/);
	assert.match(ul[0], /class="accounts"/, 'the list needs the class popup.css styles');
	assert.match(css, /\.accounts\b/, 'the stylesheet must target that class');
	// The counts strip: three cells, each named by an aria string main already built, and the note
	// line written as text.
	assert.match(script, /button\.setAttribute\('aria-label', cell\.aria\)/, 'a cell name comes from the ready-to-print aria string');
	assert.match(script, /noteEl\.textContent/, 'the note line is filled with textContent');
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
	assert.match(script, /startDisabled = Boolean\(view\.startDisabled\)/);
	assert.match(script, /toggleBtn\.disabled = [^;]*startDisabled/);
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

// setSetting is the one write path from a page: main checks the sender and the allow-list before it
// touches the theme, the settings file or the login item.
test('setSetting is validated in main before anything is written', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	const start = main.indexOf('CHANNEL.setSetting');
	const handler = main.slice(start, main.indexOf('ipcMain.handle(CHANNEL.getStatus', start));
	assert.match(handler, /!fromUs\(event\.senderFrame\) \|\| !isAllowedSetting\(name, value\)\) return null;[\s\S]*setLoginItemSettings/);
});

// The popup shows the engine state too (its pause/start button and status line), so main must push
// every state change to both pages, and the popup must read the current state on load.
test('engine state is pushed to the window and the popup, and the popup reads it on load', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.ok(main.includes('currentWindow?.webContents.send(CHANNEL.state'), 'state goes to the window');
	assert.ok(main.includes('popupWindow?.webContents.send(CHANNEL.state'), 'state goes to the popup');
	const popup = readFileSync(join(DESKTOP, 'renderer', 'popup.js'), 'utf8');
	assert.ok(popup.includes('api.getState()') && popup.includes('api.onState('), 'the popup bootstraps and subscribes');
});

// The accounts screen writes the user's config, so the two new calls come from the window page only
// (the popup must not read or write it), every edit is validated in main, and the password is never
// part of an argv: it is written to the child's stdin.
test('getConfig and editAccount are window-only, validated in main, and the password goes to stdin', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.ok(main.includes("senderFrame?.url === RENDERER_URL;"), 'fromWindow names the window page only');
	for (const channel of ['getConfig', 'editAccount']) {
		const start = main.indexOf(`CHANNEL.${channel}`);
		assert.ok(start > 0, `${channel} has a handler`);
		assert.match(main.slice(start, start + 200), /!fromWindow\(event\.senderFrame\)\) return null/, `${channel} must refuse other senders`);
	}
	assert.ok(/buildEditRequests\(op, fields\)/.test(main), 'editAccount validates through buildEditRequests');
	assert.ok(main.includes('engine.runConfig(request.args, request.stdin'), 'the password is passed as stdin');
	const engine = readFileSync(join(SRC, 'engine.ts'), 'utf8');
	assert.ok(engine.includes('child.stdin.end(input)'), 'stdin is written and closed');
	const renderer = readFileSync(join(DESKTOP, 'renderer', 'renderer.js'), 'utf8');
	assert.equal(/\.innerHTML\s*=/.test(renderer), false);
});

// The app bar carries the app name, so markup and CSS are pinned together: the window page says
// the name exactly once, and the rule that centres it on the whole bar (with 132px clear on each
// side) is the executable version of the design's AppBar title span. The popup keeps its own lockup.
test('the app bar names the app once, and only the window has it', () => {
	const index = readFileSync(join(DESKTOP, 'renderer', 'index.html'), 'utf8');
	const bar = /<header class="appbar">[\s\S]*?<\/header>/.exec(index);
	assert.ok(bar, 'index.html must keep the app bar header');
	assert.equal((bar[0].match(/<span class="appbar-title">triage mail<\/span>/g) ?? []).length, 1, 'the bar names the app exactly once');
	assert.match(bar[0], /<svg class="emblem"/, 'the emblem stays in the bar');
	assert.equal(bar[0].includes('<h1>'), false, 'the bar carries no heading; each screen has its own h1');
	const popup = readFileSync(join(DESKTOP, 'renderer', 'popup.html'), 'utf8');
	assert.ok(!popup.includes('appbar-title'), 'the popup keeps its own lockup, not the app bar');
});

test('the app-bar title is centred on the whole bar and keeps 132px clear each side', () => {
	const css = readFileSync(join(DESKTOP, 'renderer', 'renderer.css'), 'utf8');
	const title = /\.appbar-title\s*\{([^}]*)\}/.exec(css);
	assert.ok(title, 'renderer.css must style the app-bar title');
	for (const decl of [
		'position: absolute;',
		'left: 50%;',
		'top: 50%;',
		'transform: translate(-50%, -50%);',
		'max-width: calc(100% - 264px);',
		'overflow: hidden;',
		'text-overflow: ellipsis;',
		'white-space: nowrap;',
		'font-size: var(--item);',
		'font-weight: 500;',
		'line-height: 1.35;',
		'color: var(--fg);',
		'pointer-events: none;',
	]) {
		assert.ok(title[1].includes(decl), `.appbar-title must declare ${decl}`);
	}
	const bar = /\.appbar\s*\{([^}]*)\}/.exec(css);
	assert.ok(bar, 'renderer.css must style the app bar');
	assert.ok(bar[1].includes('position: relative;'), 'the bar anchors the absolutely positioned title');
	assert.ok(bar[1].includes('justify-content: flex-end;'), 'the emblem stays at the right end');
	const tokens = readFileSync(join(DESKTOP, 'renderer', 'tokens.css'), 'utf8');
	assert.ok(tokens.includes('--item: 15px;'), 'tokens.css defines the item size the title uses');
});

// The counts strip is a UI surface Piotr must be able to reach by keyboard and hear by name. The
// executable statement: three native buttons (no tabindex tricks), a visible 3px focus ring on a cell,
// and the decorative dot/arrow hidden from assistive tech (the count lives in the aria-label instead).
test('the counts strip is three keyboard buttons with a focus ring and names that hold the count', () => {
	const html = readFileSync(join(DESKTOP, 'renderer', 'popup.html'), 'utf8');
	const css = readFileSync(join(DESKTOP, 'renderer', 'popup.css'), 'utf8');
	const strip = /<div id="strip"[\s\S]*?<\/div>/.exec(html);
	assert.ok(strip, 'popup.html must contain the counts strip');
	assert.equal((strip[0].match(/<button[^>]*class="cell"/g) ?? []).length, 3, 'exactly three count buttons');
	assert.equal(/tabindex/.test(strip[0]), false, 'no manual tab order: DOM order is the tab order');
	assert.match(css, /\.cell:focus-visible\s*\{[^}]*outline:\s*3px solid/, 'a focused cell shows a 3px ring');
	assert.ok(/<span class="dot" aria-hidden="true">/.test(html), 'the needs-reply dot is aria-hidden');
	assert.match(html, /<svg class="icon"[^>]*aria-hidden="true"><use href="#i-arrow-up-right"/, 'the corner arrow is aria-hidden');
});

// First-run onboarding: "install Ollama" may open exactly one fixed page in the user's browser, and the
// notification switches become Electron Notifications in main only — notifier.ts decides whether and
// how many, never displays anything, and no page can reach either call.
test('setup opens one fixed URL, and Notification is built in main only', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.equal((main.match(/shell\.openExternal\(/g) ?? []).length, 1, 'openExternal appears exactly once in main');
	assert.match(main, /shell\.openExternal\(OLLAMA_INSTALL_URL\)/, 'the one call uses the fixed constant, not a page value');
	const setup = readFileSync(join(SRC, 'setup.ts'), 'utf8');
	assert.match(setup, /OLLAMA_INSTALL_URL = 'https:\/\/ollama\.com\/download'/, 'the one URL, spelled once, in the pure module');
	for (const file of ['notifier.ts', 'setup.ts', 'setup-model.ts', 'engine.ts', 'ipc.ts', 'settings.ts']) {
		const text = readFileSync(join(SRC, file), 'utf8');
		assert.equal(/new Notification\(/.test(text), false, `${file} must not build a Notification`);
	}
	assert.ok(/new Notification\(/.test(main), 'main is where a Notification is built');
	for (const page of ['renderer.js', 'popup.js']) {
		const script = readFileSync(join(DESKTOP, 'renderer', page), 'utf8');
		assert.equal(/openExternal|new Notification/.test(script), false, `${page} can reach neither`);
		assert.equal(/\.innerHTML\s*=/.test(script), false, `${page} must render with textContent, never an innerHTML sink`);
	}
});

// The setup channel drives engine writes and settings writes, so only the window page may use it:
// the popup answers idle. The two notification switches are rows on the settings screen (the board's
// answer), and a click writes them back through the allow-listed setSetting.
test('ltm.setup is the window page only, and the settings screen carries the two switches', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	const start = main.indexOf('ipcMain.handle(CHANNEL.setup');
	assert.ok(start > 0, 'the setup channel has a handler');
	assert.match(main.slice(start, start + 260), /!fromWindow\(event\.senderFrame\)\) return \{ kind: 'idle' \}/, 'the popup never drives setup');
	const index = readFileSync(join(DESKTOP, 'renderer', 'index.html'), 'utf8');
	assert.ok(index.includes('id="notifyReply"') && index.includes('id="notifyOffline"'), 'the two switch rows exist');
	const renderer = readFileSync(join(DESKTOP, 'renderer', 'renderer.js'), 'utf8');
	assert.ok(renderer.includes("setSetting('notifyReply'") && renderer.includes("setSetting('notifyOffline'"), 'the rows write back');
	assert.equal(/setSetting\('setupDone'/.test(renderer), false, 'setupDone is main-only: the page cannot mark onboarding done');
});

// The first-run orchestration is in the DOM/Electron layer, which has no browser unit test here, so its
// wiring is pinned structurally: detection starts on the model step and re-checks/refreshes while visible;
// mode/digest Continue submit the chosen values (not an immediate write on a click); the chosen model and
// typed address feed the requests; a completed pull is saved; and the download bar never reads as full.
test('the onboarding steps drive the engine and settings end-to-end', () => {
	const renderer = readFileSync(join(DESKTOP, 'renderer', 'renderer.js'), 'utf8');
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');

	// 1. Detection starts when the model step paints its "looking for a runtime" placeholder, once, and while
	// visible the step re-probes a not-found (every 3 s) and refreshes a running download.
	assert.ok(renderer.includes("view.pill === 'looking for a runtime'") && renderer.includes('&& !setupDetectStarted'), 'entering the model step starts a single detect');
	assert.ok(renderer.includes("api.setup('detect', { url: setupAddress })"), 'the model step and not-found re-check send a detect with the typed address');
	assert.ok(renderer.includes('}, 3_000);'), 'a three-second interval keeps the model step current while visible');

	// 2. Mode Continue submits the selected choice; a segment click only records the choice locally.
	assert.ok(renderer.includes("setupChosenMode = value"), 'a mode click is a local choice');
	assert.ok(renderer.includes(".setup('mode', { choice })"), 'Continue submits the selected mode');
	assert.equal(/renderSetupMode[\s\S]{0,400}api\.setup\('mode', \{ choice: value \}/.test(renderer), false, 'a click must not run the engine write by itself');

	// 6. Digest Continue writes the digest and the notification switches before marking setup complete.
	assert.ok(renderer.includes("api.setup('digest', { enabled: setupDigestEnabled"), 'the digest switch reaches main on Continue');
	assert.ok(renderer.includes("api.setup('settings', { notifyReply: setupNotifyReply"), 'the notification switches reach main on Continue');
	assert.ok(renderer.includes("api.setup('start', {})"), 'and only then setup finishes');

	// 8. The chosen model and the typed runtime address feed save/pull/detect, and the address is read live.
	assert.ok(renderer.includes("api.setup('save', { model: setupChosenModel"), 'Continue saves the chosen model');
	assert.ok(renderer.includes("url: setupAddress }"), 'requests carry the typed runtime address');
	assert.ok(renderer.includes('setupAddress = input.value.trim()'), 'the address field is read live into setupAddress');

	// 10. The bar keeps its maximum at 100 and the fill is the clamped percent (never a partial "full").
	assert.ok(renderer.includes("bar.setAttribute('aria-valuemax', '100')"), 'the bar maximum stays 100');
	assert.ok(renderer.includes("fill.style.setProperty('--fill'"), 'the fill takes the clamped percent');
	assert.equal(/aria-valuemax', String\(pct\)/.test(renderer), false, 'the maximum is never the current percent');
	const css = readFileSync(join(DESKTOP, 'renderer', 'onboarding.css'), 'utf8');
	assert.match(css, /\.model-panel \.progress-fill \{[^}]*width: var\(--fill, 100%\)/, 'the fill width is the driven percentage');

	// 9. A completed pull in main saves the exact runtime/URL/model it downloaded (a download records the model).
	const doneIdx = main.indexOf("session.pullState = 'done'");
	assert.ok(doneIdx > 0, 'a successful pull marks the step done');
	assert.match(main.slice(doneIdx, doneIdx + 400), /buildModelSaveArgs\(saveRuntime, saveUrl, model\)/, 'the pulled model is saved');

	// 3. The live notification switches hydrate from saved settings, and a silent skip persists both false.
	assert.ok(main.includes('notifySwitches.notifyReply = settings.notifyReply'), 'boot hydrates the switches from settings');
	assert.ok(main.includes('settings = skippedInstallSettings(settings)'), 'a silent-skip install turns both switches off');
});
