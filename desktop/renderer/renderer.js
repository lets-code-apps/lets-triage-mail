// The main window page. It has no Node and no Electron (contextIsolation + sandbox); the only
// bridge is the nine functions the preload exposed as `window.triageMail`: getState, onState,
// onCheckResult, command, getStatus, getSettings, setSetting, getConfig and editAccount.
//
// Everything the engine reports is hostile data (config names, engine sentences, server and folder
// names, engine log lines), so it is always written with textContent on elements made by
// createElement, never as markup. The strings were built in the main process (popup-model.ts); a
// pill's colour class comes only from the allow-list below. Account edits go through editAccount
// (main validates them and the engine writes the config); this page never sees a config file, and a
// typed password is cleared from its field as soon as the call returns.

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

// The three modes, as the engine words them, with the config keys each one sets. Flagging is set
// before dry_run so an account is never live with the wrong flagging.
const MODES = [
	{
		word: 'dry-run',
		label: 'dry run',
		note: 'Nothing changes on the server; results go to the digest.',
		changes: [{ key: 'dry_run', value: true }],
	},
	{
		word: 'classify-only',
		label: 'classify only',
		note: 'Mail is classified for the digest; no flags are set on the server.',
		changes: [
			{ key: 'flagging', value: false },
			{ key: 'dry_run', value: false },
		],
	},
	{
		word: 'live',
		label: 'classify and sort',
		note: 'Mail is classified and flag colours are set on the server. Nothing is moved or deleted.',
		changes: [
			{ key: 'flagging', value: true },
			{ key: 'dry_run', value: false },
		],
	},
];

let section = 'accounts';
let engineView = null;
let statusPayload = null;
let checkResult = null;
let selected = null; // account name
let config = null; // the trimmed `config show` (or its error), from getConfig
let adding = false; // the detail pane shows the add form
let busy = false; // an account call is running
let detailKey = ''; // what the detail pane was built from; it is only rebuilt when this changes
let passwordOpen = false;
const testReports = {}; // account name -> the last test result shown in its detail
let logOpen = false;
let checking = null; // { startedAt } while a check runs
let checkTimer = null;
let checkedAt = ''; // HH:MM of the last result
let showResults = true;

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
	checkedAt = new Date().toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', hour12: false });
	showResults = true; // a fresh result is shown; hiding it is the user's call
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

// The status rows and the config's accounts, joined by account name. The status wins for state,
// pill, note and counts; an account that is only in the config (just added, engine not restarted
// yet) gets a muted "not running" pill.
function mergedAccounts(view) {
	const fromConfig = config && config.ok ? config.accounts : [];
	const rows = [];
	for (const c of fromConfig) {
		const st = view.accounts.find((a) => a.name === c.name);
		rows.push({
			name: c.name,
			user: c.username,
			host: c.host,
			port: c.port,
			tls: c.tls,
			mode: st ? st.mode : c.mode,
			state: st ? st.state : 'not running',
			tone: st ? st.tone : 'muted',
			note: st ? st.note : '',
			counts: st ? st.counts : '',
			initials: st ? st.initials : initialsOf(c.name),
		});
	}
	for (const st of view.accounts) {
		if (!rows.some((r) => r.name === st.name)) rows.push({ ...st, user: '', host: '', port: 0, tls: '' });
	}
	return rows;
}

// First letters of the first two words, like the popup's avatar.
const initialsOf = (name) =>
	name
		.split(/\s+/)
		.filter(Boolean)
		.slice(0, 2)
		.map((word) => [...word][0].toUpperCase())
		.join('');

function renderAccounts() {
	const view = accountsView();
	summaryEl.textContent = view.summary;
	// The 5 s poll rebuilds the rows; keep keyboard focus on the same account.
	const focused = rowsEl.contains(document.activeElement) ? document.activeElement.dataset.name : undefined;
	clear(rowsEl);

	const accounts = mergedAccounts(view);
	if (!accounts.some((a) => a.name === selected)) selected = accounts.length > 0 ? accounts[0].name : null;

	for (const account of accounts) {
		const row = el('button', 'row');
		row.type = 'button';
		row.dataset.name = account.name;
		row.title = account.note;
		row.setAttribute('aria-pressed', String(!adding && account.name === selected));
		const line = el('div', 'line');
		line.append(el('span', 'name', account.name), el('span', 'meta', account.mode));
		const text = el('div', 'text');
		text.append(line);
		if (account.user) text.append(el('span', 'meta user', account.user));
		text.append(el('span', 'meta note', account.note), pill(account.tone, account.state));
		row.append(el('span', 'avatar', account.initials), text, icon('chevron-right', 16));
		row.addEventListener('click', () => {
			selected = account.name;
			adding = false;
			passwordOpen = false;
			renderAccounts();
		});
		rowsEl.append(row);
		if (account.name === focused) row.focus();
	}
	// With no accounts at all the only useful detail is the add form.
	const showAdd = adding || (config !== null && accounts.length === 0);
	const account = accounts.find((a) => a.name === selected);
	const key = showAdd ? 'add' : JSON.stringify(account ?? null) + (config && !config.ok ? config.error : '');
	// The detail holds form fields: a poll must not wipe what is being typed or a call in flight.
	if (key === detailKey || busy || (passwordOpen && !showAdd)) return;
	detailKey = key;
	clear(detailEl);
	if (showAdd) renderAddForm();
	else if (account) renderAccountDetail(account);
	if (config && !config.ok && !config.missing) detailEl.append(el('p', 'meta form-error', config.error));
}

// --- the accounts forms ------------------------------------------------------------------

// A labelled input with an optional helper line (real label/for, helper tied by aria-describedby).
function field(id, label, attrs, helper) {
	const wrap = el('div', 'field');
	const labelEl = el('label', '', label);
	labelEl.htmlFor = id;
	const input = el('input');
	input.id = id;
	for (const [k, v] of Object.entries(attrs)) input.setAttribute(k, v);
	wrap.append(labelEl, input);
	if (helper) {
		const note = el('p', 'helper', helper);
		note.id = id + '-help';
		input.setAttribute('aria-describedby', note.id);
		wrap.append(note);
	}
	return { wrap, input };
}

// A button whose label can swap to "adding…" with the busy dot while its call runs.
function button(label, kind, onClick) {
	const b = el('button', 'btn ' + kind);
	b.type = 'button';
	b.append(el('span', '', label));
	b.addEventListener('click', onClick);
	return b;
}

// Runs one account call: every control in the detail pane is disabled meanwhile, the pressed
// button shows its busy label, and the result (or a thrown error) comes back as a plain object.
async function runCall(pressed, busyLabel, call) {
	busy = true;
	const controls = [...detailEl.querySelectorAll('button, input')];
	for (const c of controls) c.disabled = true;
	const label = pressed.querySelector('span');
	const original = label.textContent;
	label.textContent = busyLabel;
	pressed.prepend(el('span', 'busy-dot'));
	let result;
	try {
		result = await call();
	} catch (failed) {
		result = { ok: false, error: String(failed) };
	}
	busy = false;
	// The page may have been rebuilt meanwhile (selection change): only touch nodes still attached.
	for (const c of controls) c.disabled = false;
	label.textContent = original;
	pressed.querySelector('.busy-dot')?.remove();
	return result ?? { ok: false, error: 'no answer' };
}

function setMessage(node, text, isError) {
	node.textContent = text;
	node.classList.toggle('form-error', Boolean(isError));
}

const restartNote = (result) => (result.restart === 'external' ? ' Restart triage to apply.' : '');

// Check items in the settings check's own rendering.
function appendChecks(container, report) {
	const items = (report && report.items) || [];
	if (items.length === 0) container.append(el('p', '', 'All checks passed.'));
	for (const item of items) {
		const row = el('div');
		row.append(pill(CHECK_TONES[item.status], String(item.status)), el('span', 'area', item.area), el('span', 'msg', item.message));
		if (item.hint) row.append(el('span', 'msg meta', item.hint));
		container.append(row);
	}
}

function reloadAccounts() {
	detailKey = '';
	return loadConfig().then(() => {
		refresh();
		renderAccounts();
	});
}

function renderAddForm() {
	const form = el('form', 'account-form');
	form.noValidate = true;
	form.addEventListener('submit', (event) => event.preventDefault());
	form.append(el('h2', '', 'add account'));
	const name = field('f-name', 'name', { type: 'text', autocomplete: 'off', maxlength: '64' });
	const host = field('f-host', 'IMAP server', { type: 'text', autocomplete: 'off', placeholder: 'imap.example.com' });
	const port = field('f-port', 'port', { type: 'number', min: '1', max: '65535', value: '993', inputmode: 'numeric' });
	port.input.setAttribute('autocomplete', 'off');
	const user = field('f-user', 'user', { type: 'email', autocomplete: 'username' });
	const pass = field('f-pass', 'password', { type: 'password', autocomplete: 'current-password' }, 'If your provider uses app passwords, paste one here.');
	const server = el('div', 'field-row');
	server.append(host.wrap, port.wrap);
	host.wrap.classList.add('grow');
	const message = el('p', 'form-msg');
	message.setAttribute('aria-live', 'polite');
	const results = el('div', 'checks');
	results.setAttribute('aria-live', 'polite');

	const values = () => ({
		name: name.input.value.trim(),
		host: host.input.value.trim(),
		username: user.input.value.trim(),
		port: port.input.value === '' ? undefined : Number(port.input.value),
		password: pass.input.value,
	});
	const complete = (v) => v.host && v.username && v.password;
	let lastTest = null;

	const test = button('test connection', 'secondary', async () => {
		const v = values();
		if (!complete(v)) return setMessage(message, 'Fill in the server, user and password to test.', true);
		setMessage(message, '', false);
		clear(results);
		const result = await runCall(test, 'testing…', () => api.editAccount('test', { host: v.host, port: v.port, username: v.username, password: v.password }));
		if (result.report) {
			lastTest = result.report;
			appendChecks(results, result.report);
		} else setMessage(message, String(result.error), true);
	});
	const add = button('add account', 'primary', async () => {
		const v = values();
		if (!v.name || !complete(v)) return setMessage(message, 'Fill in the name, server, user and password.', true);
		setMessage(message, '', false);
		const result = await runCall(add, 'adding…', () => api.editAccount('add', v));
		pass.input.value = ''; // the password does not stay in the page
		if (!result.ok) return setMessage(message, String(result.error), true);
		if (lastTest) testReports[v.name] = { report: lastTest };
		selected = v.name;
		adding = false;
		await reloadAccounts();
		setMessage(accountMsg(), 'Added.' + restartNote(result), false);
	});
	add.prepend(icon('plus', 18));
	const buttons = el('div', 'btns');
	buttons.append(add, test);
	const note = el('p', 'meta', 'New accounts start in dry run: nothing changes on the server until you change the mode.');
	form.append(name.wrap, server, user.wrap, pass.wrap, buttons, message, results, note);
	detailEl.append(form);
}

// The message line of the selected account's detail (it exists once the detail was rebuilt).
const accountMsg = () => detailEl.querySelector('.form-msg') ?? el('p');

function renderAccountDetail(account) {
	detailEl.append(el('h2', '', account.name), pill(account.tone, account.state), el('p', 'meta', account.user));
	detailEl.append(el('p', '', account.note), el('p', '', account.counts));
	const message = el('p', 'form-msg');
	message.setAttribute('aria-live', 'polite');

	// mode
	const modes = el('section', 'group');
	const modeLabel = el('h3', '', 'mode');
	modeLabel.id = 'lbl-mode';
	const seg = el('div', 'seg');
	seg.setAttribute('role', 'radiogroup');
	seg.setAttribute('aria-labelledby', 'lbl-mode');
	const current = MODES.find((m) => m.word === account.mode);
	const modeNote = el('p', 'meta', current ? current.note : '');
	for (const mode of MODES) {
		const b = el('button');
		b.append(el('span', '', mode.label));
		b.type = 'button';
		b.setAttribute('role', 'radio');
		b.setAttribute('aria-checked', String(mode === current));
		b.addEventListener('click', async () => {
			if (mode === current) return;
			const result = await runCall(b, mode.label, () => api.editAccount('set', { name: account.name, changes: mode.changes }));
			await reloadAccounts();
			setMessage(accountMsg(), result.ok ? 'Mode saved.' + restartNote(result) : String(result.error), !result.ok);
		});
		seg.append(b);
	}
	modes.append(modeLabel, seg, modeNote);

	// connection
	const connection = el('section', 'group');
	connection.append(el('h3', '', 'connection'));
	for (const [label, value] of [['host', account.host], ['port', account.port ? String(account.port) : ''], ['user', account.user], ['tls', account.tls]]) {
		if (value) connection.append(el('p', 'meta kv', label + ': ' + value));
	}
	const results = el('div', 'checks');
	results.setAttribute('aria-live', 'polite');
	const shown = testReports[account.name];
	if (shown) appendChecks(results, shown.report);
	const test = button('test connection', 'secondary', async () => {
		clear(results);
		const result = await runCall(test, 'testing…', () => api.editAccount('test', { name: account.name }));
		if (result.report) {
			testReports[account.name] = { report: result.report };
			appendChecks(results, result.report);
		} else setMessage(message, String(result.error), true);
	});
	connection.append(test, results);

	// password
	const password = el('section', 'group');
	password.append(el('h3', '', 'password'));
	const open = button('update password', 'secondary', () => {
		passwordOpen = true;
		open.hidden = true;
		const pass = field('f-newpass', 'new password', { type: 'password', autocomplete: 'new-password' }, "Stored in your Mac's Keychain. Plain letters, digits and symbols only.");
		const save = button('save', 'primary', async () => {
			const result = await runCall(save, 'saving…', () => api.editAccount('password', { name: account.name, password: pass.input.value }));
			pass.input.value = '';
			if (!result.ok) return setMessage(message, String(result.error), true);
			passwordOpen = false;
			await reloadAccounts();
			setMessage(accountMsg(), 'Password saved.' + restartNote(result), false);
		});
		const cancel = button('cancel', 'ghost', () => {
			pass.input.value = '';
			passwordOpen = false;
			detailKey = '';
			renderAccounts();
		});
		const row = el('div', 'btns');
		row.append(save, cancel);
		password.append(pass.wrap, row);
		pass.input.focus();
	});
	password.append(open);

	// remove
	const remove = button('remove account', 'ghost danger', async () => {
		const result = await runCall(remove, 'removing…', () => api.editAccount('remove', { name: account.name }));
		if (!result.ok) return setMessage(message, result.error === 'Cancelled.' ? '' : String(result.error), true);
		delete testReports[account.name];
		selected = null;
		await reloadAccounts();
	});
	detailEl.append(modes, connection, password, remove, message);
}

// The engine row: its state line (with the bounded stderr tail the engine printed before dying)
// and one button, "pause triage" while the app's engine runs, otherwise "start triage"; busy
// while starting or stopping.
function renderEngine() {
	if (!engineView) return;
	const tail = Array.isArray(engineView.failureTail) ? engineView.failureTail : [];
	const state = 'Engine: ' + String(engineView.state);
	const retry = engineView.restartInSeconds == null ? '' : '\nrestarting in ' + engineView.restartInSeconds + ' s…';
	$('engineNote').textContent = (tail.length > 0 ? state + ' — ' + tail.join('\n') : state) + retry;
	const running = engineView.state === 'running';
	const busy = engineView.state === 'starting' || engineView.state === 'stopping';
	$('toggleLabel').textContent = busy ? (engineView.state === 'starting' ? 'starting…' : 'pausing…') : running ? 'pause triage' : 'start triage';
	toggleBtn.querySelector('use').setAttribute('href', running ? '#i-pause' : '#i-play');
	toggleBtn.querySelector('.icon').toggleAttribute('hidden', busy); // SVG elements have no .hidden property
	toggleBtn.querySelector('.busy-dot').hidden = !busy;
	const idle = engineView.state === 'stopped' || engineView.state === 'failed';
	toggleBtn.disabled = busy || (!running && (!idle || Boolean(accountsView().startDisabled)));
}

const plural = (n, word) => n + ' ' + word + (n === 1 ? '' : 's');

// The setup check row: the note, the main button and the results toggle follow one of three
// states: never run, running (with an elapsed-seconds counter), or done.
function renderCheckRow() {
	const note = $('checkNote');
	const button = $('checkbtn');
	button.disabled = checking !== null;
	button.querySelector('.busy-dot').hidden = checking === null;
	$('resultsbtn').hidden = checking !== null || !checkResult;
	$('resultsbtn').textContent = showResults ? 'hide results' : 'show results';
	checksEl.hidden = !showResults;
	if (checking !== null) {
		const n = accountsView().accounts.length;
		const seconds = Math.floor((Date.now() - checking.startedAt) / 1000);
		$('checkLabel').textContent = 'checking…';
		note.textContent = 'Checking the model and ' + (n > 0 ? plural(n, 'account') : 'accounts') + '… ' + seconds + ' s';
	} else if (!checkResult) {
		$('checkLabel').textContent = 'check setup';
		note.textContent = 'Validates config, model and accounts.';
	} else {
		$('checkLabel').textContent = 'check again';
		const report = checkResult.report;
		note.textContent = checkResult.error
			? 'Check failed'
			: 'Checked at ' + checkedAt + (report ? ' · ' + plural(report.warnings, 'warning') + ', ' + report.failed + ' failed' : '');
	}
}

function renderChecks() {
	clear(checksEl);
	renderCheckRow();
	if (!checkResult) return;
	if (checkResult.error) {
		checksEl.append(el('p', '', 'Check failed: ' + String(checkResult.error)));
		return;
	}
	appendChecks(checksEl, checkResult.report);
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

// The config's accounts (host, user, mode) for the list and the detail; a failure is kept as a message.
function loadConfig() {
	return Promise.resolve(api.getConfig()).then(
		(c) => {
			if (c) config = c;
		},
		(failed) => {
			config = { ok: false, error: String(failed), missing: false };
		},
	);
}

// One round-trip: ask, paint. Main answers `idle` while the window is hidden.
function refresh() {
	void loadConfig().then(() => {
		if (section === 'accounts') renderAccounts();
	});
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
$('addbtn').addEventListener('click', () => {
	adding = true;
	passwordOpen = false;
	renderAccounts();
	const first = document.getElementById('f-name');
	if (first) first.focus();
});
toggleBtn.addEventListener('click', () => {
	void api.command(engineView && engineView.state === 'running' ? 'stop' : 'start').then((r) => {
		renderState(r.view);
		refresh();
	});
});
$('checkbtn').addEventListener('click', () => {
	checking = { startedAt: Date.now() };
	checkTimer = setInterval(renderCheckRow, 1000);
	renderCheckRow();
	// Whatever happens, the busy state ends with the answer.
	const done = (result) => {
		clearInterval(checkTimer);
		checking = null;
		renderCheck(result ?? { error: 'no answer' });
	};
	void Promise.resolve(api.command('check')).then(done, (failed) => done({ error: String(failed) }));
});
$('resultsbtn').addEventListener('click', () => {
	showResults = !showResults;
	renderCheckRow();
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
