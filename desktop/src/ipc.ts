// The one place that names the messages crossing the main <-> renderer boundary.
//
// Renderer code (and the tests) import these types and constants; the main process imports
// them too, so a renamed channel or a drifted payload is a compile error, not a silent
// mismatch. This module is deliberately free of any `electron` import so it can be unit-tested
// under plain `node --test` (no GUI, no Electron runtime).

import type { CheckReport } from './check-report.ts';
import type { StatusReport } from './status-report.ts';
import type { PopupView } from './popup-model.ts';
import type { Appearance } from './settings.ts';

// Engine lifecycle states, mirrored from docs/DESKTOP.md. `running` means the process is
// alive, NOT that mail is healthy; health comes from a `check` (see engine.ts).
export type EngineState = 'stopped' | 'starting' | 'running' | 'stopping' | 'failed';

// A command the renderer can ask the main process to run. Nothing here reaches mail, the
// model or the database: each maps to one engine subcommand, or to a window/app action main
// owns. `openWindow` and `quit` exist so the popup's footer buttons need no extra channel.
// `openMail` is the popup's three count buttons: it runs a fixed `open -a Mail` (Apple Mail
// itself, no folder) and takes no argument, so it still needs no new channel or preload function.
export type EngineCommand = 'start' | 'stop' | 'check' | 'openWindow' | 'openSettings' | 'openMail' | 'quit';

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
	// Seconds until the next automatic restart while `failed`, else null (see engine.ts).
	restartInSeconds: number | null;
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
	getStatus: 'ltm.getStatus',
	getSettings: 'ltm.getSettings',
	setSetting: 'ltm.setSetting',
	getConfig: 'ltm.getConfig',
	editAccount: 'ltm.editAccount',
	setup: 'ltm.setup',
} as const;

// First-run onboarding. One channel carries every step the window page drives; setup does not add
// engine commands to `EngineCommand` (those ride `ltm.setup`), and no new channel is created for
// the popup's `openMail`. Types live here so main and the tests agree on the shape by construction.

// The four onboarding steps, in fixed order. The page owns the step number; main answers a view
// for a step the page asks about.
export type SetupStep = 'accounts' | 'mode' | 'model' | 'digest';

// An operation the window page asks the main process to run during setup. Renderer strings never
// become flags: this is a closed set, mapped to a fixed engine argv in main, and the password (when
// there is one) goes to the child's stdin, never to argv.
export type SetupOp =
	| 'view'
	| 'accounts'
	| 'addAccount'
	| 'mode'
	| 'decline'
	| 'detect'
	| 'pull'
	| 'save'
	| 'modelRetry'
	| 'open'
	| 'cancelDownload'
	| 'digest'
	| 'settings'
	| 'start'
	| 'back';

// One of the four-tone colours the popup allow-list already uses (popup-model.ts). Engine text only
// ever carries these four; a fifth is a bug, not a style.
export type Tone = 'positive' | 'attention' | 'danger' | 'muted';

// A ready-to-print view for one onboarding step: strings and a tone, built in main so the sandboxed
// page never formats or colours anything itself. Optional fields are simply absent for steps that do
// not use them (fields/options/actions/progress). Continue is disabled by `nextOff`.
export type SetupView = {
	step: SetupStep;
	index: number; // 0-based step number
	title: string;
	text: string;
	pill?: string;
	tone?: Tone;
	body?: string;
	muted?: string;
	note?: string;
	fields?: { label: string; kind: 'text' | 'password'; placeholder?: string; helper?: string; example?: string; value?: string }[];
	options?: { value: string; label: string }[];
	selected?: string;
	actions?: { label: string; glyph?: string; variant?: 'primary' | 'secondary' }[];
	progress?: { label: string; value: number; max: number; pct: number } | null;
	nextLabel?: string;
	nextVariant?: 'primary' | 'secondary';
	nextOff?: boolean;
};

// What one setup op answers the page. `idle` is main's answer when the window is not the requester
// (the popup must never drive setup). `done` marks the final "start triage".
export type SetupResult =
	| { kind: 'view'; view: SetupView }
	| { kind: 'done' }
	| { kind: 'error'; message: string }
	| { kind: 'idle' };

// What the window's settings screen shows. `section` is a one-shot hint: "settings" once after the
// popup asked to open the window there (the window reads it on focus and main clears it). The three
// setup keys are filled once onboarding is wired in main (they are optional so the settings screen
// and its shot fixtures stay as they are until then).
export type SettingsPayload = {
	appearance: Appearance;
	openAtLogin: boolean;
	log: string[];
	section: 'settings' | null;
	setupDone?: boolean;
	notifyReply?: boolean;
	notifyOffline?: boolean;
};

// Result of a `command` invocation: the command echoed back plus the state it produced.
export type CommandResult = { command: EngineCommand; view: EngineView; report?: CheckView | null; error?: string | null };

// Result of running `check`: either a parsed report (render as data) or a clean error string.
// Defined here (not in engine.ts) so the payload helper below can stay electron-free and unit-testable.
export type CheckResult = { kind: 'report'; report: CheckReport } | { kind: 'error'; message: string };

// Result of one `status --json` read: the parsed report, or a clean error string. Also declared
// here so `engine.ts` and the popup's reader agree on it by construction.
export type StatusResult = { kind: 'status'; report: StatusReport } | { kind: 'error'; message: string };

// What the popup page actually receives. `idle` is main's answer while the popup is hidden: an
// invisible page must not spawn the engine, so polling stops with the window, not with a timer.
// `view` is a ready-to-print `PopupView` (strings + a tone) built in main by `popup-model.ts`, so
// the sandboxed page never formats or colours anything itself.
export type StatusPayload =
	| { kind: 'status'; view: PopupView }
	| { kind: 'idle' }
	| { kind: 'error'; message: string };

// Turn a status outcome into the exact payload the popup renders. Pure and typed, so main can
// neither drop the view nor forget the error (same reasoning as `checkPayload` above).
export function statusPayload(result: StatusResult, view: PopupView): StatusPayload {
	return result.kind === 'status' ? { kind: 'status', view } : { kind: 'error', message: result.message };
}

// The idle answer, spelled in one place so the popup's own reader has one shape to handle.
export const IDLE_PAYLOAD: StatusPayload = { kind: 'idle' };

// Turn a check outcome into the exact payload the renderer renders. Pure and typed, so a main-process
// test cannot silently drop the report or the error; both main.ts call sites (the tray and the
// renderer's own Check button) build their result through this one function.
export function checkPayload(command: EngineCommand, view: EngineView, result: CheckResult): CommandResult {
	return result.kind === 'report'
		? { command, view, report: result.report, error: null }
		: { command, view, report: null, error: result.message };
}
