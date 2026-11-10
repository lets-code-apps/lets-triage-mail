// The main window page. It has no Node and no Electron (contextIsolation + sandbox); the only
// bridge is the seven functions the preload exposed as `window.triageMail`: getState, onState,
// onCheckResult, command, getStatus, getSettings and setSetting.
//
// Everything the engine reports is hostile data (config names, engine sentences, server and folder
// names, engine log lines), so it is always written with textContent on elements made by
// createElement, never as markup. The strings were built in the main process (popup-model.ts); a
// pill's colour class comes only from the allow-list below. The window only shows what the engine
// reports: no config writing, no forms.

const api = window.triageMail;

const TONES = ['positive', 'attention', 'danger', 'muted'];
// Check item status -> pill tone.
const CHECK_TONES = { ok: 'positive', warn: 'attention', fail: 'danger' };
const REFRESH_MS = 5_000;
const SVG_NS = 'http://www.w3.org/2000/svg';

const $ = (id) => document.getElementById(id);
const summaryEl = $('summary');
const rowsEl = $('rows');
const detailEl = $('detail');
const checksEl = $('checks');
const toggleBtn = $('toggle');
const logEl = $('log');
const logBtn = $('logbtn');

const ACCOUNTS_HINT = 'Accounts come from the config file. Edit it, then use check setup in settings.';

let section = 'accounts';
let engineView = null;
let statusPayload = null;
let checkResult = null;
let selected = null; // account name
let logOpen = false;

const clear = (node) => {
	while (node.firstChild !== null) node.removeChild(node.firstChild);
};

function el(tag, className, text) {
	const node = document.createElement(tag);
	if (className) node.className = className;
	if (text !== undefined) node.textContent = text;
	return node;
}

// A design glyph from the sprite in index.html.
function icon(name, px) {
	const svg = document.createElementNS(SVG_NS, 'svg');
	svg.setAttribute('class', 'icon');
	svg.setAttribute('width', String(px));
	svg.setAttribute('height', String(px));
	svg.setAttribute('aria-hidden', 'true');
	const use = document.createElementNS(SVG_NS, 'use');
	use.setAttribute('href', '#i-' + name);
	svg.append(use);
	return svg;
}

// Only a name from the allow-list may become a class; anything else gets the neutral style.
function pill(tone, text) {
	return el('span', TONES.includes(tone) ? 'pill ' + tone : 'pill', text);
}

function renderState(view) {
	if (!view) return;
	engineView = view;
	renderEngine();
	if (logOpen) loadSettings(); // the log changes with the engine
}

function renderCheck(result) {
	if (!result) return;
	checkResult = result;
	renderChecks();
	show('settings');
}

function show(next) {
	section = next;
	render();
}

function render() {
	for (const button of document.querySelectorAll('.nav button')) {
		if (button.dataset.section === section) button.setAttribute('aria-current', 'page');
		else button.removeAttribute('aria-current');
	}
	const accounts = section === 'accounts';
	$('list').hidden = !accounts;
	detailEl.hidden = !accounts;
	$('settings').hidden = accounts;
	if (accounts) renderAccounts();
	else renderEngine();
}

// What the status view says about the accounts, or the engine's own error line.
function accountsView() {
	if (!statusPayload || statusPayload.kind === 'idle') return { summary: '', accounts: [] };
	if (statusPayload.kind === 'error') return { summary: String(statusPayload.message), accounts: [] };
	return statusPayload.view;
}

function renderAccounts() {
	const view = accountsView();
	summaryEl.textContent = view.summary;
	// The 5 s poll rebuilds the rows; keep keyboard focus on the same account.
	const focused = rowsEl.contains(document.activeElement) ? document.activeElement.dataset.name : undefined;
	clear(rowsEl);
	clear(detailEl);

	const accounts = view.accounts;
	if (!accounts.some((a) => a.name === selected)) selected = accounts.length > 0 ? accounts[0].name : null;

	for (const account of accounts) {
		const row = el('button', 'row');
		row.type = 'button';
		row.dataset.name = account.name;
		row.title = account.note;
		row.setAttribute('aria-pressed', String(account.name === selected));
		const line = el('div', 'line');
		line.append(el('span', 'name', account.name), el('span', 'meta', account.mode));
		const text = el('div', 'text');
		text.append(line, el('span', 'meta note', account.note), pill(account.tone, account.state));
		row.append(el('span', 'avatar', account.initials), text, icon('chevron-right', 16));
		row.addEventListener('click', () => {
			selected = account.name;
			renderAccounts();
		});
		rowsEl.append(row);
		if (account.name === focused) row.focus();
	}

	const account = accounts.find((a) => a.name === selected);
	if (account) {
		detailEl.append(el('h2', '', account.name), pill(account.tone, account.state), el('p', 'meta', account.mode));
		detailEl.append(el('p', '', account.note), el('p', '', account.counts));
	}
	detailEl.append(el('p', 'meta', ACCOUNTS_HINT));
}

// The engine row: its state line (with the bounded stderr tail the engine printed before dying)
// and one button, "pause triage" while the app's engine runs, otherwise "start triage".
function renderEngine() {
	if (!engineView) return;
	const tail = Array.isArray(engineView.failureTail) ? engineView.failureTail : [];
	const state = 'Engine: ' + String(engineView.state);
	$('engineNote').textContent = tail.length > 0 ? state + ' — ' + tail.join('\n') : state;
	const running = engineView.state === 'running';
	$('toggleLabel').textContent = running ? 'pause triage' : 'start triage';
	toggleBtn.firstElementChild.hidden = !running; // the pause glyph
	const idle = engineView.state === 'stopped' || engineView.state === 'failed';
	toggleBtn.disabled = !running && (!idle || Boolean(accountsView().startDisabled));
}

function renderChecks() {
	clear(checksEl);
	if (!checkResult) return;
	if (checkResult.error) {
		checksEl.append(el('p', '', 'Check failed: ' + String(checkResult.error)));
		return;
	}
	const items = (checkResult.report && checkResult.report.items) || [];
	if (items.length === 0) checksEl.append(el('p', '', 'All checks passed.'));
	for (const item of items) {
		const row = el('div');
		row.append(pill(CHECK_TONES[item.status], String(item.status)), el('span', 'area', item.area), el('span', 'msg', item.message));
		if (item.hint) row.append(el('span', 'msg meta', item.hint));
		checksEl.append(row);
	}
}

// Paint the settings main reported: appearance, open at login, the log, and a one-shot "open on
// settings" hint from the popup's settings button.
function applySettings(s) {
	if (!s) return;
	for (const button of document.querySelectorAll('#appearance button')) {
		button.setAttribute('aria-checked', String(button.dataset.value === s.appearance));
	}
	$('login').checked = Boolean(s.openAtLogin);
	logEl.textContent = Array.isArray(s.log) ? s.log.join('\n') : '';
	if (s.section === 'settings') show('settings');
}

function loadSettings() {
	void Promise.resolve(api.getSettings()).then(applySettings);
}

// One round-trip: ask, paint. Main answers `idle` while the window is hidden.
function refresh() {
	void Promise.resolve(api.getStatus()).then(
		(payload) => {
			if (!payload || payload.kind === 'idle') return;
			statusPayload = payload;
			render();
		},
		(failed) => {
			statusPayload = { kind: 'error', message: 'Status unavailable: ' + String(failed) };
			render();
		},
	);
}

// Bootstrap the CURRENT engine state on load, then subscribe to changes. Without the getState() call,
// a window opened while the engine is already running would show a blank state with both controls
// enabled until the next transition. onCheckResult lets the tray's "Check setup" (which this page
// did not trigger) show up here too.
void api.getState().then(renderState);
api.onState(renderState);
api.onCheckResult(renderCheck);

for (const button of document.querySelectorAll('.nav button')) {
	button.addEventListener('click', () => show(button.dataset.section));
}
for (const button of document.querySelectorAll('#appearance button')) {
	button.addEventListener('click', () => {
		void api.setSetting('appearance', button.dataset.value).then(applySettings);
	});
}
$('login').addEventListener('change', (event) => {
	void api.setSetting('openAtLogin', event.target.checked).then(applySettings);
});
toggleBtn.addEventListener('click', () => {
	void api.command(engineView && engineView.state === 'running' ? 'stop' : 'start').then((r) => {
		renderState(r.view);
		refresh();
	});
});
$('checkbtn').addEventListener('click', () => {
	void api.command('check').then(renderCheck);
});
logBtn.addEventListener('click', () => {
	logOpen = !logOpen;
	logEl.hidden = !logOpen;
	logBtn.textContent = logOpen ? 'hide log' : 'show log';
	logBtn.setAttribute('aria-expanded', String(logOpen));
	if (logOpen) loadSettings();
});

// A window shown again (or focused after the popup's settings button) repaints now, not in 5 s.
document.addEventListener('visibilitychange', () => {
	if (document.visibilityState === 'visible') {
		refresh();
		loadSettings();
	}
});
window.addEventListener('focus', loadSettings);

render();
refresh();
loadSettings();
setInterval(() => {
	if (document.visibilityState === 'visible') refresh();
}, REFRESH_MS);
