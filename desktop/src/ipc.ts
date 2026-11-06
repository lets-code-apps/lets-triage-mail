// The one place that names the messages crossing the main <-> renderer boundary.
//
// Renderer code (and the tests) import these types and constants; the main process imports
// them too, so a renamed channel or a drifted payload is a compile error, not a silent
// mismatch. This module is deliberately free of any `electron` import so it can be unit-tested
// under plain `node --test` (no GUI, no Electron runtime).

import type { CheckReport } from './check-report.ts';

// Engine lifecycle states, mirrored from docs/DESKTOP.md. `running` means the process is
// alive, NOT that mail is healthy; health comes from a `check` (see engine.ts).
export type EngineState = 'stopped' | 'starting' | 'running' | 'stopping' | 'failed';

// A command the renderer can ask the main process to run. Nothing here reaches mail, the
// model or the database: each maps to one engine subcommand.
export type EngineCommand = 'start' | 'stop' | 'check';

// What the renderer shows about the engine. `exitCode`/`signal` are filled only in `failed`.
export type EngineView = {
	state: EngineState;
	version: string;
	exitCode: number | null;
	signal: string | null;
};

// A full check report is sent to the renderer as data only. It carries server and folder names,
// so it must be rendered with textContent, never as HTML (enforced in renderer.js).
export type CheckView = { ok: boolean; failed: number; warnings: number; items: CheckReport['items'] };

// IPC channel names. One constant per channel keeps the three exposed functions and their
// `ipcMain.handle`/`ipcRenderer.invoke` pairs in sync.
export const CHANNEL = {
	getState: 'ltm.getState',
	state: 'ltm.state',
	command: 'ltm.command',
} as const;

// Result of a `command` invocation: the command echoed back plus the state it produced.
export type CommandResult = { command: EngineCommand; view: EngineView; report?: CheckView | null; error?: string | null };
