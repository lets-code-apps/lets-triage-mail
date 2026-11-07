// The menu-bar popup page. Sandbox + contextIsolation, so the only bridge is the five preload
// functions on `window.triageMail`: getState, onState, onCheckResult, command and getStatus.
//
// Every string here was already built in the main process by `popup-model.ts`, so this file
// assigns `textContent` and never formats, translates or colours anything itself. Engine output
// is hostile data (config names, engine sentences): no `innerHTML`, and a pill's colour class
// comes only from the four names on `TONES` below — anything else gets no class at all.

const api = window.triageMail;

// A pill may only be coloured by one of these four class names (see popup.css).
const TONES = ['positive', 'attention', 'danger', 'muted'];

// How often the popup asks the engine for a new snapshot. `status --json` reads a local
// SQLite file, so a poll every few seconds is cheap; while the popup is hidden the main
// process answers `idle` and spawns nothing at all.
const REFRESH_MS = 5_000;

const generatedEl = document.getElementById('generated');
const countEl = document.getElementById('count');
const countLabelEl = document.getElementById('countLabel');
const sublineEl = document.getElementById('subline');
const summaryEl = document.getElementById('summary');
const accountsEl = document.getElementById('accounts');
const startBtn = document.getElementById('start');

// `render` only paints what main already built; the buttons below are the popup's own actions.
function render(payload) {
  if (!payload || payload.kind === 'idle') return; // hidden: nothing to paint

  if (payload.kind === 'error') {
    // The engine's own reason, as one line of text.
    summaryEl.textContent = String(payload.message);
    countEl.textContent = '—';
    while (accountsEl.firstChild !== null) accountsEl.removeChild(accountsEl.firstChild);
    return;
  }

  const view = payload.view;
  generatedEl.textContent = view.generated;
  countEl.textContent = view.headline;
  countLabelEl.textContent = view.headlineLabel;
  sublineEl.textContent = view.subline;
  summaryEl.textContent = view.summary;
  // main already decided whether Start is safe (a fresh status may show an engine this app did
  // not start); the page only reflects that, it never decides it.
  startBtn.disabled = Boolean(view.startDisabled);

  // Rebuild the rows: the account list can change (a config edit), and it is short.
  while (accountsEl.firstChild !== null) accountsEl.removeChild(accountsEl.firstChild);
  for (const account of view.accounts) {
    const row = document.createElement('li');

    const name = document.createElement('span');
    name.className = 'name';
    name.textContent = account.name;

    const mode = document.createElement('span');
    mode.className = 'mode';
    mode.textContent = account.mode;

    const note = document.createElement('span');
    note.className = 'note';
    note.textContent = account.note;

    const pill = document.createElement('span');
    // Only a name from the allow-list may become a class; unknown tones get the neutral style.
    pill.className = TONES.includes(account.tone) ? `pill ${account.tone}` : 'pill';
    pill.textContent = account.state;

    row.appendChild(name);
    row.appendChild(mode);
    row.appendChild(note);
    row.appendChild(pill);
    accountsEl.appendChild(row);
  }
}

// One round-trip: ask, paint. The `void` keeps the unhandled-rejection rule honest.
function refresh() {
  void Promise.resolve(api.getStatus()).then(render, (failed) => {
    summaryEl.textContent = `Status unavailable: ${String(failed)}`;
  });
}

document.getElementById('start').addEventListener('click', () => {
  void api.command('start').then(refresh);
});
document.getElementById('stop').addEventListener('click', () => {
  void api.command('stop').then(refresh);
});
document.getElementById('check').addEventListener('click', () => {
  void api.command('check').then(refresh);
});
document.getElementById('window').addEventListener('click', () => {
  void api.command('openWindow').then(refresh);
});
// Quit ends the app (and stops the engine); the popup closes with it.
document.getElementById('quit').addEventListener('click', () => {
  void api.command('quit');
});

// Paint once at once, then keep the snapshot fresh for as long as the page is alive.
refresh();
setInterval(refresh, REFRESH_MS);
