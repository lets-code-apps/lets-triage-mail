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

const stripEl = document.getElementById('strip');
const cells = [...stripEl.querySelectorAll('.cell')];
const connectedEl = document.getElementById('connected');
const accountsEl = document.getElementById('accounts');
const pausedPillEl = document.getElementById('pausedPill');
const noteEl = document.getElementById('note');
const toggleBtn = document.getElementById('toggle');
const toggleLabel = document.getElementById('toggleLabel');

let engineView = { state: 'stopped', failureTail: [] };
let startDisabled = false;
// Whether the app considers the engine paused (Stop pressed this session): shows the "paused" pill and
// makes the footer button read "resume triage". main decides it; the page only reflects it.
let paused = false;

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

// The footer's one engine button, the same as the window's: "pause triage" while the app's engine
// runs, otherwise "start triage"; busy while starting or stopping; disabled when main says an
// outside engine is live (a second one would double-watch the mailbox).
function renderEngine() {
	const state = engineView.state;
	const busy = state === 'starting' || state === 'stopping';
	const running = state === 'running';
	// "resume triage" is the paused copy: the engine is down because Stop was pressed here, so the next
	// press resumes rather than cold-starts. Anything else that is not running still says "start triage".
	toggleLabel.textContent = busy ? (state === 'starting' ? 'starting…' : 'pausing…') : running ? 'pause triage' : paused ? 'resume triage' : 'start triage';
	toggleBtn.querySelector('use').setAttribute('href', running ? '#i-pause' : '#i-play');
	toggleBtn.querySelector('.icon').toggleAttribute('hidden', busy); // SVG elements have no .hidden property
	toggleBtn.querySelector('.busy-dot').hidden = !busy;
	toggleBtn.disabled = busy || (!running && startDisabled);

	toggleBtn.title = !running && startDisabled ? 'Triage is running outside the app.' : '';
}

// `render` only paints what main already built; the buttons below are the popup's own actions.
function render(payload) {
	if (!payload || payload.kind === 'idle') return; // hidden: nothing to paint

	if (payload.kind === 'error') {
		// No trustworthy numbers: hide the whole strip and the paused pill, keep the engine's reason on
		// the note line (the old subline's job), and drop the account rows.
		stripEl.hidden = true;
		pausedPillEl.hidden = true;
		noteEl.textContent = String(payload.message);
		connectedEl.textContent = '';
		clear();
		return;
	}
	stripEl.hidden = false;

	const view = payload.view;
	// Paint the three cells from the strip main already built: the number, the label, the needs-reply
	// dot (shown only for the allow-listed 'attention' tone), and the accessible name that repeats the
	// count. The page formats nothing itself.
	for (let i = 0; i < cells.length && i < view.strip.length; i += 1) {
		const cell = view.strip[i];
		const button = cells[i];
		button.querySelector('.num').textContent = cell.count;
		button.querySelector('.lbl').textContent = cell.label;
		const dot = button.querySelector('.dot');
		if (dot) dot.hidden = cell.tone !== 'attention';
		button.setAttribute('aria-label', cell.aria);
	}
	// The paused pill and the note line are main's words; the page only shows/hides and assigns text.
	paused = Boolean(view.paused);
	pausedPillEl.hidden = !paused;
	noteEl.textContent = view.note;
	connectedEl.textContent = view.summary;
	// main already decided whether Start is safe (a fresh status may show an engine this app did
	// not start); the page only reflects that, it never decides it.
	startDisabled = Boolean(view.startDisabled);
	renderEngine();

	// Rebuild the rows: the account list can change (a config edit), and it is short.
	clear();
	for (const account of view.accounts) {
		const row = document.createElement('li');
		row.className = 'row';
		row.title = account.note;

		const text = document.createElement('div');
		text.className = 'text';
		const line = document.createElement('div');
		line.className = 'line';
		line.append(span('name', account.name), span('meta', account.mode));
		text.append(line, span('meta note', account.note), span(pillClass(account.tone), account.state));

		// Only a name from the allow-list may become a class; unknown tones get the neutral style.
		row.append(span('avatar', account.initials), text);
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
	engineView = view;
	renderEngine();
}

// Run a command, then repaint: the engine state arrives through onState, the numbers through refresh.
const run = (command) => () => {
	void api.command(command).then(refresh);
};

toggleBtn.addEventListener('click', () => run(engineView.state === 'running' ? 'stop' : 'start')());
document.getElementById('open').addEventListener('click', run('openWindow'));
// All three count buttons do the same thing: open Apple Mail itself via the fixed `openMail` command.
// The cell is not identified to main — there is no folder or per-message target, by design.
for (const cell of cells) cell.addEventListener('click', run('openMail'));
document.getElementById('settings').addEventListener('click', run('openSettings'));
// Quit ends the app (and stops the engine); the popup closes with it.
document.getElementById('quit').addEventListener('click', () => {
	void api.command('quit');
});

renderEngine();
void api.getState().then(renderState);
api.onState(renderState);

// Paint once at once, then keep the snapshot fresh for as long as the page is alive.
refresh();
setInterval(refresh, REFRESH_MS);
