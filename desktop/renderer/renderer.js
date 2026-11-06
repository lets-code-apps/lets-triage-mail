// The renderer process. It has no Node and no Electron (contextIsolation + sandbox); the only
// bridge is the three functions the preload exposed as `window.triageMail`.
//
// Everything the engine reports is hostile data (server and folder names from a mailbox we do
// not own), so it is always written with textContent, never innerHTML. This page is a static
// placeholder for the real screens, not the accounts/folders/settings views.

const api = window.triageMail;

const stateEl = document.getElementById('state');
const checkEl = document.getElementById('check');

// Engine state, rendered as text only.
function renderState(view) {
	if (!view) return;
	stateEl.textContent = 'Engine: ' + String(view.state);
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

api.onState(renderState);

document.getElementById('start').addEventListener('click', () => {
	void api.command('start').then((r) => renderState(r.view));
});
document.getElementById('stop').addEventListener('click', () => {
	void api.command('stop').then((r) => renderState(r.view));
});
document.getElementById('checkbtn').addEventListener('click', () => {
	void api.command('check').then((r) => renderCheck(r));
});
