// The renderer process. It has no Node and no Electron (contextIsolation + sandbox); the only
// bridge is the five functions the preload exposed as `window.triageMail`: getState, onState,
// onCheckResult, command and getStatus (the popup's poll, which this page does not use).
//
// Everything the engine reports is hostile data (server and folder names from a mailbox we do
// not own), so it is always written with textContent, never innerHTML. This page is a static
// placeholder for the real screens, not the accounts/folders/settings views.

const api = window.triageMail;

const stateEl = document.getElementById('state');
const checkEl = document.getElementById('check');

// Engine state, rendered as text only. On a failure we also show the bounded stderr tail the engine
// printed before dying (a missing config, an unreachable server) — still textContent, never HTML.
function renderState(view) {
	if (!view) return;
	const tail = Array.isArray(view.failureTail) ? view.failureTail : [];
	stateEl.textContent = tail.length > 0 ? 'Engine: ' + String(view.state) + ' — ' + tail.join('\n') : 'Engine: ' + String(view.state);
	document.getElementById('start').disabled = view.state !== 'stopped' && view.state !== 'failed';
	document.getElementById('stop').disabled = view.state !== 'running';
}

// A check report is a list of items; each message is plain text, never markup.
function renderCheck(result) {
	if (!result) return;
	if (result.error) {
		checkEl.textContent = 'Check failed: ' + String(result.error);
		return;
	}
	const items = (result.report && result.report.items) || [];
	const parts = [];
	for (const item of items) {
		let line = '[' + item.status.toUpperCase() + '] ' + item.area + ' — ' + item.message;
		if (item.hint) line += ' → ' + item.hint;
		parts.push(line);
	}
	checkEl.textContent = parts.length === 0 ? 'All checks passed.' : parts.join('\n');
}

// Bootstrap the CURRENT engine state on load, then subscribe to changes. Without the getState() call,
// a window opened while the engine is already running would show a blank state with both controls
// enabled until the next transition. onCheckResult lets the tray's "Check setup" (which the renderer
// did not trigger) show up here too.
void api.getState().then(renderState);
api.onState(renderState);
api.onCheckResult(renderCheck);

document.getElementById('start').addEventListener('click', () => {
	void api.command('start').then((r) => renderState(r.view));
});
document.getElementById('stop').addEventListener('click', () => {
	void api.command('stop').then((r) => renderState(r.view));
});
document.getElementById('checkbtn').addEventListener('click', () => {
	void api.command('check').then((r) => renderCheck(r));
});
