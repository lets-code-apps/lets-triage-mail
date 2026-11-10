// The preload bridge. With `contextIsolation: true` and `sandbox: true` the renderer has no Node
// and no `electron`; it can only call what we expose. Sandboxed preload scripts must be CommonJS,
// which is why this is a `.cjs` using `require('electron')` rather than an ESM module.
//
// We expose exactly seven functions: getState, onState, onCheckResult, getStatus, command, getSettings and
// setSetting (main validates the setting name and value).
// `onCheckResult` is a push channel (no round-trip) so the tray's "Check setup" can deliver a check
// to a renderer that did not ask for one; `getStatus` is the popup's own poll, answered `idle` by
// main while the popup is hidden. The channel strings must match src/ipc.ts (CHANNEL) and main.ts's
// ipcMain registrations; test/boundaries.test.ts keeps them in sync.

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
	getStatus: () => ipcRenderer.invoke('ltm.getStatus'),
	command: (cmd) => ipcRenderer.invoke('ltm.command', cmd),
	getSettings: () => ipcRenderer.invoke('ltm.getSettings'),
	setSetting: (name, value) => ipcRenderer.invoke('ltm.setSetting', name, value),
});
