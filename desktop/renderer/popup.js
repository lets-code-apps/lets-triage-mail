// The menu-bar popup page. Sandbox + contextIsolation, so the only bridge is the seven preload
// functions on `window.triageMail`; this page uses getStatus, getState, onState and command.
//
// Every string here was already built in the main process by `popup-model.ts`, so this file
// assigns `textContent` and never formats, translates or colours anything itself. Engine output
// is hostile data (config names, engine sentences): no `innerHTML`, and a pill's colour class
// comes only from the four names on `TONES` below — anything else gets no class at all.

const api = window.triageMail;

// A pill may only be coloured by one of these four class names (see shared.css).
const TONES = ['positive', 'attention', 'danger', 'muted'];

// How often the popup asks the engine for a new snapshot. `status --json` reads a local
// SQLite file, so a poll every few seconds is cheap; while the popup is hidden the main
// process answers `idle` and spawns nothing at all.
const REFRESH_MS = 5_000;

const countEl = document.getElementById('count');
const pillEl = document.getElementById('pill');
const sublineEl = document.getElementById('subline');
const connectedEl = document.getElementById('connected');
const accountsEl = document.getElementById('accounts');
const toggleBtn = document.getElementById('toggle');
const toggleLabel = document.getElementById('toggleLabel');

let engineState = 'stopped';
let startDisabled = false;

const pillClass = (tone) => (TONES.includes(tone) ? 'pill ' + tone : 'pill');

const clear = () => {
	while (accountsEl.firstChild !== null) accountsEl.removeChild(accountsEl.firstChild);
};

function span(className, text) {
	const node = document.createElement('span');
	node.className = className;
	node.textContent = text;
	return node;
}

// The footer's one engine button: "pause triage" while the app's engine runs, otherwise "start
// triage", disabled while it is changing state or when main says an outside engine is live.
function renderToggle() {
	const running = engineState === 'running';
	toggleLabel.textContent = running ? 'pause triage' : 'start triage';
	toggleBtn.firstElementChild.hidden = !running; // the pause glyph
	const idle = engineState === 'stopped' || engineState === 'failed';
	toggleBtn.disabled = !running && (!idle || startDisabled);
}

// `render` only paints what main already built; the buttons below are the popup's own actions.
function render(payload) {
	if (!payload || payload.kind === 'idle') return; // hidden: nothing to paint

	if (payload.kind === 'error') {
		// The engine's own reason, as one line of text.
		countEl.textContent = '—';
		pillEl.className = pillClass('muted');
		pillEl.textContent = 'not running';
		sublineEl.textContent = String(payload.message);
		connectedEl.textContent = '';
		clear();
		return;
	}

	const view = payload.view;
	countEl.textContent = view.headline;
	pillEl.className = pillClass(view.pillTone);
	pillEl.textContent = view.pill;
	sublineEl.textContent = view.subline;
	connectedEl.textContent = view.summary;
	// main already decided whether Start is safe (a fresh status may show an engine this app did
	// not start); the page only reflects that, it never decides it.
	startDisabled = Boolean(view.startDisabled);
	renderToggle();

	// Rebuild the rows: the account list can change (a config edit), and it is short.
	clear();
	for (const account of view.accounts) {
		const row = document.createElement('li');
		row.className = 'row';

		const text = document.createElement('div');
		text.className = 'text';
		const line = document.createElement('div');
		line.className = 'line';
		line.append(span('name', account.name), span('meta', account.mode));
		text.append(line, span('meta note', account.note));

		// Only a name from the allow-list may become a class; unknown tones get the neutral style.
		row.append(span('avatar', account.initials), text, span(pillClass(account.tone), account.state));
		accountsEl.appendChild(row);
	}
}

// One round-trip: ask, paint. The `void` keeps the unhandled-rejection rule honest.
function refresh() {
	void Promise.resolve(api.getStatus()).then(render, (failed) => {
		sublineEl.textContent = `Status unavailable: ${String(failed)}`;
	});
}

function renderState(view) {
	if (!view) return;
	engineState = view.state;
	renderToggle();
}

// Run a command, then repaint: the engine state arrives through onState, the numbers through refresh.
const run = (command) => () => {
	void api.command(command).then(refresh);
};

toggleBtn.addEventListener('click', () => run(engineState === 'running' ? 'stop' : 'start')());
document.getElementById('open').addEventListener('click', run('openWindow'));
document.getElementById('summary').addEventListener('click', run('openWindow'));
document.getElementById('settings').addEventListener('click', run('openSettings'));
// Quit ends the app (and stops the engine); the popup closes with it.
document.getElementById('quit').addEventListener('click', () => {
	void api.command('quit');
});

void api.getState().then(renderState);
api.onState(renderState);

// Paint once at once, then keep the snapshot fresh for as long as the page is alive.
refresh();
setInterval(refresh, REFRESH_MS);
