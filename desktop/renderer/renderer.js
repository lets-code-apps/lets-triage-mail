// The main window page. It has no Node and no Electron (contextIsolation + sandbox); the only
// bridge is the five functions the preload exposed as `window.triageMail`: getState, onState,
// onCheckResult, command and getStatus.
//
// Everything the engine reports is hostile data (config names, engine sentences, server and folder
// names), so it is always written with textContent on elements made by createElement, never as
// markup. The strings were built in the main process (popup-model.ts); a pill's colour class comes
// only from the allow-list below. The window only shows what the engine reports: no config
// writing, no forms.

const api = window.triageMail;

const TONES = ['positive', 'attention', 'danger', 'muted'];
// Check item status -> pill tone.
const CHECK_TONES = { ok: 'positive', warn: 'attention', fail: 'danger' };
const REFRESH_MS = 5_000;

const titleEl = document.getElementById('title');
const summaryEl = document.getElementById('summary');
const rowsEl = document.getElementById('rows');
const buttonsEl = document.getElementById('buttons');
const detailEl = document.getElementById('detail');
const startBtn = document.getElementById('start');
const stopBtn = document.getElementById('stop');

const NO_CHECK = 'Press Check setup to validate your setup.';
const ACCOUNTS_HINT = 'Accounts come from the config file. Edit it, then use Check setup in settings.';

let section = 'accounts';
let engineView = null;
let statusPayload = null;
let checkResult = null;
let selected = null; // account name

const clear = (node) => {
	while (node.firstChild !== null) node.removeChild(node.firstChild);
};

function el(tag, className, text) {
	const node = document.createElement(tag);
	if (className) node.className = className;
	if (text !== undefined) node.textContent = text;
	return node;
}

// Only a name from the allow-list may become a class; anything else gets the neutral style.
function pill(tone, text) {
	return el('span', TONES.includes(tone) ? 'pill ' + tone : 'pill', text);
}

function renderState(view) {
	if (!view) return;
	engineView = view;
	renderSettings();
}

function renderCheck(result) {
	if (!result) return;
	checkResult = result;
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
	buttonsEl.hidden = section !== 'settings';
	if (section === 'settings') renderSettings();
	else renderAccounts();
}

// What the status view says about the accounts, or the engine's own error line.
function accountsView() {
	if (!statusPayload || statusPayload.kind === 'idle') return { summary: '', accounts: [] };
	if (statusPayload.kind === 'error') return { summary: String(statusPayload.message), accounts: [] };
	return statusPayload.view;
}

function renderAccounts() {
	const view = accountsView();
	titleEl.textContent = 'accounts';
	summaryEl.textContent = view.summary;
	// The 5 s poll rebuilds the rows; keep keyboard focus on the same account.
	const focused = rowsEl.contains(document.activeElement) ? document.activeElement.dataset.name : undefined;
	clear(rowsEl);
	clear(detailEl);

	const accounts = view.accounts;
	if (!accounts.some((a) => a.name === selected)) selected = accounts.length > 0 ? accounts[0].name : null;

	for (const account of accounts) {
		const row = el('button');
		row.type = 'button';
		row.dataset.name = account.name;
		row.setAttribute('aria-pressed', String(account.name === selected));
		const text = el('span', 'text');
		text.append(el('span', 'name', account.name), el('span', 'meta', account.mode), el('span', 'meta', account.note));
		row.append(el('span', 'avatar', account.initials), text, pill(account.tone, account.state));
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
	detailEl.append(el('p', 'hint', ACCOUNTS_HINT));
}

// The engine state line keeps the bounded stderr tail the engine printed before dying.
function renderSettings() {
	if (section !== 'settings') return;
	titleEl.textContent = 'settings';
	clear(rowsEl);
	if (engineView) {
		const tail = Array.isArray(engineView.failureTail) ? engineView.failureTail : [];
		const state = 'Engine: ' + String(engineView.state);
		summaryEl.textContent = tail.length > 0 ? state + ' — ' + tail.join('\n') : state;
		startBtn.disabled =
			(engineView.state !== 'stopped' && engineView.state !== 'failed') || Boolean(accountsView().startDisabled);
		stopBtn.disabled = engineView.state !== 'running';
	}

	clear(detailEl);
	detailEl.append(el('h2', '', 'setup check'));
	if (!checkResult) {
		detailEl.append(el('p', 'hint', NO_CHECK));
		return;
	}
	if (checkResult.error) {
		detailEl.append(el('p', '', 'Check failed: ' + String(checkResult.error)));
		return;
	}
	const items = (checkResult.report && checkResult.report.items) || [];
	if (items.length === 0) detailEl.append(el('p', '', 'All checks passed.'));
	const list = el('div', 'checks');
	for (const item of items) {
		const row = el('div');
		row.append(pill(CHECK_TONES[item.status], String(item.status)), el('span', 'area', item.area), el('span', 'msg', item.message));
		if (item.hint) row.append(el('span', 'msg meta', item.hint));
		list.append(row);
	}
	detailEl.append(list);
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
startBtn.addEventListener('click', () => {
	void api.command('start').then((r) => {
		renderState(r.view);
		refresh();
	});
});
stopBtn.addEventListener('click', () => {
	void api.command('stop').then((r) => renderState(r.view));
});
document.getElementById('checkbtn').addEventListener('click', () => {
	void api.command('check').then(renderCheck);
});

document.addEventListener('visibilitychange', () => {
	if (document.visibilityState === 'visible') refresh();
});

render();
refresh();
setInterval(() => {
	if (document.visibilityState === 'visible') refresh();
}, REFRESH_MS);
