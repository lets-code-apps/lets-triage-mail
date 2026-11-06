// The preload bridge. With `contextIsolation: true` and `sandbox: true` the renderer has no Node
// and no `electron`; it can only call what we expose. Sandboxed preload scripts must be CommonJS,
// which is why this is a `.cjs` using `require('electron')` rather than an ESM module.
//
// We expose exactly four functions: getState, onState, onCheckResult, command. `onCheckResult` is a
// push channel (no round-trip) so the tray's "Check setup" can deliver a check to a renderer that
// did not ask for one. The channel strings must match src/ipc.ts (CHANNEL) and main.ts's ipcMain
// registrations; a test in test/boundaries.test.ts keeps them in sync.

const { contextBridge, ipcRenderer } = require('electron');

contextBridge.exposeInMainWorld('triageMail', {
	getState: () => ipcRenderer.invoke('ltm.getState'),
	onState: (listener) => {
		const wrapped = (_event, view) => listener(view);
		ipcRenderer.on('ltm.state', wrapped);
		return () => ipcRenderer.removeListener('ltm.state', wrapped);
	},
	onCheckResult: (listener) => {
		const wrapped = (_event, result) => listener(result);
		ipcRenderer.on('ltm.checkResult', wrapped);
		return () => ipcRenderer.removeListener('ltm.checkResult', wrapped);
	},
	command: (cmd) => ipcRenderer.invoke('ltm.command', cmd),
});
