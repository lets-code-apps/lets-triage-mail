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

// What the renderer shows about the engine. `exitCode`/`signal`/`failureTail` are filled only in
// `failed`. `failureTail` is the last few stderr lines the engine printed before it died (bounded
// in engine.ts) so a failed start shows the actionable reason, not just an exit code. It is engine
// output, so the renderer must write it with textContent, never as HTML.
export type EngineView = {
	state: EngineState;
	version: string;
	exitCode: number | null;
	signal: string | null;
	failureTail: string[];
};

// A full check report is sent to the renderer as data only. It carries server and folder names,
// so it must be rendered with textContent, never as HTML (enforced in renderer.js).
export type CheckView = { ok: boolean; failed: number; warnings: number; items: CheckReport['items'] };

// IPC channel names. One constant per channel keeps the exposed functions and their
// `ipcMain.handle`/`ipcRenderer.invoke` pairs in sync. `checkResult` is a push channel (not an
// invoke round-trip): it lets the tray's "Check setup" deliver a finished check to a renderer that
// did not ask for it.
export const CHANNEL = {
	getState: 'ltm.getState',
	state: 'ltm.state',
	command: 'ltm.command',
	checkResult: 'ltm.checkResult',
} as const;

// Result of a `command` invocation: the command echoed back plus the state it produced.
export type CommandResult = { command: EngineCommand; view: EngineView; report?: CheckView | null; error?: string | null };

// Result of running `check`: either a parsed report (render as data) or a clean error string.
// Defined here (not in engine.ts) so the payload helper below can stay electron-free and unit-testable.
export type CheckResult = { kind: 'report'; report: CheckReport } | { kind: 'error'; message: string };

// Turn a check outcome into the exact payload the renderer renders. Pure and typed, so a main-process
// test cannot silently drop the report or the error; both main.ts call sites (the tray and the
// renderer's own Check button) build their result through this one function.
export function checkPayload(command: EngineCommand, view: EngineView, result: CheckResult): CommandResult {
	return result.kind === 'report'
		? { command, view, report: result.report, error: null }
		: { command, view, report: null, error: result.message };
}
