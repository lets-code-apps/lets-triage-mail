// The engine lifecycle: a small state machine around one spawned child process.
//
// This is the whole "desktop owns the process, engine owns the triage" contract in code:
// the controller spawns `lets-triage-mail`, watches its stdio and exit, and never reads the
// mail, the model, the database or the config file. `spawn`, the stop timeout and the resolver
// are injected so the tests drive it with a fake child and a short timeout (no real signals,
// no real timing).
//
// Lifecycle (docs/DESKTOP.md):
//   stopped -> starting -> running            (the `spawn` event moves starting -> running)
//   running --stop--> stopping -> stopped     (one SIGTERM, then SIGKILL after the deadline)
//   running/starting --unexpected exit--> failed (exit code + signal + last stderr)
//   failed --backoff timer--> starting        (1 s, 2 s, 4 s ... up to 5 restarts; not after the
//                                              "already running" exit, a stop or a quit)
//
// Why exactly one SIGTERM: `src/cli/run.rs::watch_for_signals` treats the FIRST SIGTERM as
// "cancel, let workers finish" and the SECOND as "exit now with 130, skip cleanup". A second
// SIGTERM would skip the engine's cleanup, so we send it once and escalate to SIGKILL instead.

import { spawn as nodeSpawn, type SpawnOptions } from 'node:child_process';
import { EventEmitter } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';

import { parseCheckReport } from './check-report.ts';
import { parseStatusReport } from './status-report.ts';
import { resolveEngine } from './engine-path.ts';
import type { CheckResult, EngineState, EngineView, StatusResult } from './ipc.ts';

// The slice of a child process this controller relies on. A real `ChildProcess` satisfies it;
// so does the fake used by the tests.
export interface EngineProcess extends EventEmitter {
	// Only set for one-shot calls that are given input (a password on `config account ...`).
	stdin?: NodeJS.WritableStream | null;
	stdout: NodeJS.ReadableStream | null;
	stderr: NodeJS.ReadableStream | null;
	kill(signal?: NodeJS.Signals | number): void;
}

// `config account test` signs in to a real server (up to a few steps of 15 s each); the other config
// calls only touch the file and the Keychain.
const CONFIG_TIMEOUT_MS = 10_000;
export const CONFIG_TEST_TIMEOUT_MS = 60_000;
const MAX_LOG_LINES = 500;
const MAX_STDERR_LINES = 8;
const DEFAULT_STOP_TIMEOUT_MS = 30_000;
// One-shot `status --json` reads a local SQLite file; anything slower than this is a wedged
// engine, and a popup must not wait on one.
const STATUS_TIMEOUT_MS = 10_000;
// `run` exits with this when another engine holds the database lock (src/cli/run.rs). Retrying
// would only fail again, so it is the one failure that is never restarted.
const ALREADY_RUNNING_EXIT_CODE = 75;
const MAX_RESTARTS = 5;
// A run that stayed up this long was healthy, so the next failure starts the backoff over.
const STABLE_RUN_MS = 10 * 60_000;
const MAX_BACKOFF_MS = 60_000;

// 1 s, 2 s, 4 s ... capped at 60 s.
export const backoffMs = (restarts: number): number => Math.min(1000 * 2 ** restarts, MAX_BACKOFF_MS);

// Injected so the tests drive the restart timer by hand.
export type Clock = {
	now(): number;
	setTimeout(fn: () => void, ms: number): unknown;
	clearTimeout(handle: unknown): void;
};
const realClock: Clock = {
	now: () => Date.now(),
	setTimeout: (fn, ms) => setTimeout(fn, ms),
	clearTimeout: (handle) => clearTimeout(handle as NodeJS.Timeout),
};

export class EngineController {
	private readonly env: NodeJS.ProcessEnv;
	private readonly repoRoot: string;
	private readonly spawn: SpawnLike;
	private readonly stopTimeoutMs: number;
	private readonly statusTimeoutMs: number;
	private readonly resourcesPath: string | undefined;
	private readonly clock: Clock;

	// Restart policy: consecutive restarts so far, when the current run began, the pending timer.
	private restarts = 0;
	private startedAt = 0;
	private restartTimer: unknown = null;
	private restartDelayMs: number | null = null;

	private current: EngineState = 'stopped';
	private child: EngineProcess | null = null;
	private exitCode: number | null = null;
	private signal: string | null = null;
	private version: string | null = null;

	// Long-lived `run` child plumbing.
	private pendingLine = '';
	private readonly logs: string[] = [];
	private readonly stderrLines: string[] = [];
	private exitResolve: (() => void) | null = null;
	private exitPromise: Promise<void> = Promise.resolve();
	// The single in-flight stop. Shared so a second Stop (or Stop-then-Quit) reuses it and never
	// sends a second SIGTERM (which the Rust engine reads as exit 130 + skipped cleanup).
	private stopPromise: Promise<void> | null = null;

	private readonly listeners = new Set<(view: EngineView) => void>();

	constructor(
		env: NodeJS.ProcessEnv,
		repoRoot: string,
		spawn: SpawnLike,
		// Stop deadline in ms; docs/DESKTOP.md derives 30 s from `ExitTimeOut`. Tests inject a tiny value.
		stopTimeoutMs: number = DEFAULT_STOP_TIMEOUT_MS,
		// How long one `status --json` may take before the popup gives up and kills it. Tests shorten it.
		statusTimeoutMs: number = STATUS_TIMEOUT_MS,
		// Electron sets `process.resourcesPath`; undefined when running outside a packaged build.
		resourcesPath: string | undefined = undefined,
		clock: Clock = realClock,
	) {
		this.clock = clock;
		this.env = env;
		this.repoRoot = repoRoot;
		this.spawn = spawn;
		this.stopTimeoutMs = stopTimeoutMs;
		this.statusTimeoutMs = statusTimeoutMs;
		this.resourcesPath = resourcesPath;
	}

	getState(): EngineView {
		const failed = this.current === 'failed';
		return {
			state: this.current,
			version: this.version ?? '',
			exitCode: failed ? this.exitCode : null,
			signal: failed ? this.signal : null,
			failureTail: failed
				? [...(this.exitCode === ALREADY_RUNNING_EXIT_CODE ? ['Another engine is already running.'] : []), ...this.stderrLines]
				: [],
			restartInSeconds: failed && this.restartDelayMs !== null ? Math.round(this.restartDelayMs / 1000) : null,
		};
	}

	onState(listener: (view: EngineView) => void): void {
		this.listeners.add(listener);
	}

	private setState(state: EngineState): void {
		this.current = state;
		const view = this.getState();
		for (const listener of this.listeners) listener(view);
	}

	private resolve(): EnginePathRef {
		return resolveEngine(this.env, this.resourcesPath, this.repoRoot);
	}

	// --- long-lived `run` child -------------------------------------------------------

	start(): void {
		// start while running/starting/stopping is a no-op; from stopped/failed spawn afresh.
		if (this.current === 'running' || this.current === 'starting' || this.current === 'stopping') {
			return;
		}
		this.restarts = 0; // a user start gets a full set of restarts
		this.launch();
	}

	// A failed run: schedule a restart unless the failure is final.
	private afterFailure(): void {
		if (this.clock.now() - this.startedAt >= STABLE_RUN_MS) this.restarts = 0;
		if (this.exitCode === ALREADY_RUNNING_EXIT_CODE || this.restarts >= MAX_RESTARTS) return;
		this.restartDelayMs = backoffMs(this.restarts);
		this.restarts += 1;
		this.restartTimer = this.clock.setTimeout(() => this.launch(), this.restartDelayMs);
	}

	private clearRestart(): void {
		if (this.restartTimer !== null) this.clock.clearTimeout(this.restartTimer);
		this.restartTimer = null;
		this.restartDelayMs = null;
	}

	private launch(): void {
		this.clearRestart();
		this.startedAt = this.clock.now();
		const { file, baseArgs } = this.resolve();
		// stdin stays an open pipe we never write to: when this app dies the pipe closes and the
		// engine shuts down (`--exit-on-stdin-close`), so it can never be orphaned.
		const child = this.spawn(file, [...baseArgs, 'run', '--exit-on-stdin-close'], { stdio: ['pipe', 'pipe', 'pipe'] });
		this.child = child;
		this.exitCode = null;
		this.signal = null;
		this.pendingLine = '';
		// A fresh attempt starts with an empty failure tail and no in-flight stop.
		this.stderrLines.length = 0;
		this.stopPromise = null;

		let resolveExit: () => void = () => {};
		this.exitPromise = new Promise<void>((resolve) => {
			resolveExit = resolve;
		});
		this.exitResolve = () => resolveExit();

		child.once('spawn', () => {
			if (this.current === 'starting') this.setState('running');
		});
		child.once('error', () => {
			// Spawn failed (e.g. binary vanished): a failure, not a crash.
			this.child = null;
			if (this.current !== 'stopping') {
				this.afterFailure();
				this.setState('failed');
			}
			this.exitResolve?.();
		});
		child.once('exit', (code: number | null, signal: NodeJS.Signals | null) => {
			this.flushPending();
			this.child = null;
			if (this.current === 'stopping') {
				this.setState('stopped');
			} else {
				this.exitCode = code;
				this.signal = signal ?? null;
				this.afterFailure();
				this.setState('failed');
			}
			this.exitResolve?.();
		});

		const stdout = child.stdout;
		const stderr = child.stderr;
		if (stdout) stdout.on('data', (chunk: unknown) => this.record(String(chunk), false));
		if (stderr) stderr.on('data', (chunk: unknown) => this.record(String(chunk), true));

		this.setState('starting');
	}

	// The last `MAX_LOG_LINES` engine lines, in memory only (never written to disk this increment).
	logCount(): number {
		return this.logs.length;
	}

	// A copy of the buffer, for the window's log box.
	getLog(): string[] {
		return [...this.logs];
	}

	private record(text: string, isErr: boolean): void {
		this.pendingLine += text;
		const parts = this.pendingLine.split('\n');
		this.pendingLine = parts.pop() ?? '';
		for (const line of parts) this.pushLine(line, isErr);
	}

	private flushPending(): void {
		if (this.pendingLine !== '') {
			this.pushLine(this.pendingLine, true);
			this.pendingLine = '';
		}
	}

	private pushLine(line: string, isErr: boolean): void {
		const trimmed = line.replace(/\s+$/, '');
		this.logs.push(trimmed);
		if (this.logs.length > MAX_LOG_LINES) this.logs.shift();
		if (isErr) {
			this.stderrLines.push(trimmed);
			if (this.stderrLines.length > MAX_STDERR_LINES) this.stderrLines.shift();
		}
	}

	// --- explicit stop ---------------------------------------------------------------

	stop(): Promise<void> {
		// Idempotent while stopping: a second Stop, or Quit right after Stop, reuses the in-flight
		// stop instead of sending a second SIGTERM.
		if (this.current === 'stopping' && this.stopPromise) return this.stopPromise;
		this.clearRestart(); // a stop (or the quit that calls it) cancels a pending restart
		if (this.current === 'stopped' || this.current === 'failed' || this.child === null) {
			this.setState('stopped');
			return Promise.resolve();
		}
		const child = this.child;
		this.setState('stopping');
		// One SIGTERM. A second would make the engine exit 130 and skip cleanup (see run.rs).
		child.kill('SIGTERM');

		// Race a clean exit against the stop deadline. `stop()` is really async: it awaits that race.
		const promise = this.awaitStop(child);
		this.stopPromise = promise;
		return promise;
	}

	private async awaitStop(child: EngineProcess): Promise<void> {
		const exited = this.exitPromise.then(() => 'exit' as const);
		const timedOut = delay(this.stopTimeoutMs).then(() => 'timeout' as const);
		const outcome = await Promise.race([exited, timedOut]);
		if (outcome === 'timeout' && this.child === child) {
			child.kill('SIGKILL');
			await this.exitPromise;
		}
		this.setState('stopped');
	}

	// --- one-shot commands ---------------------------------------------------------

	async runCheck(): Promise<CheckResult> {
		const collected = await this.collect(['check', '--json']);
		const parsed = parseCheckReport(collected.stdout);
		if ('error' in parsed) {
			const tail = collected.stderrTail.join('\n');
			const message = tail === '' ? parsed.error : `${parsed.error}\nstderr: ${tail}`;
			return { kind: 'error', message };
		}
		return { kind: 'report', report: parsed.report };
	}

	// One `config ...` call: `args` after `lets-triage-mail`, the optional `stdin` text written to the
	// child and closed (a password goes here and only here: never argv, never env, never logged).
	// Resolves with the engine's JSON, or `{ok:false, error}`; the engine prints JSON even when it
	// fails, so the exit code is ignored. The child's stderr is not echoed: it is one sentence at best.
	async runConfig(args: string[], stdin?: string, timeoutMs: number = CONFIG_TIMEOUT_MS): Promise<Record<string, unknown>> {
		const collected = await this.collect(['config', ...args], timeoutMs, stdin);
		try {
			const parsed: unknown = JSON.parse(collected.stdout);
			if (typeof parsed === 'object' && parsed !== null && !Array.isArray(parsed)) return parsed as Record<string, unknown>;
		} catch {
			// fall through
		}
		return { ok: false, error: collected.stderrTail[0] ?? 'the engine gave no answer' };
	}

	// `--version` prints `lets-triage-mail X.Y.Z`; the tray shows just the version token.
	async readVersion(): Promise<string> {
		const collected = await this.collect(['--version']);
		const raw = (collected.stdout.split('\n')[0] ?? '').trim();
		const match = /^lets-triage-mail\s+(.+)$/.exec(raw);
		this.version = match ? match[1] : raw;
		return this.version;
	}

	// Runs one short-lived engine call and gathers its streams. The exit code is kept (a `status`
	// failure is only interesting when stdout is empty); for `check --json` it is ignored, because
	// a failing setup still prints its report.
	private async collect(
		subArgs: string[],
		timeoutMs: number = 0,
		input?: string,
	): Promise<{ stdout: string; stderrTail: string[]; exitCode: number | null }> {
		const { file, baseArgs } = this.resolve();
		const child = this.spawn(file, [...baseArgs, ...subArgs], { stdio: [input === undefined ? 'ignore' : 'pipe', 'pipe', 'pipe'] });
		if (input !== undefined && child.stdin) {
			// An early exit closes the pipe under us; the answer is read from stdout either way.
			child.stdin.on('error', () => undefined);
			child.stdin.end(input);
		}

		let stdout = '';
		const stderrTail: string[] = [];
		let exitCode: number | null = null;
		if (child.stdout) child.stdout.on('data', (chunk: unknown) => (stdout += String(chunk)));
		if (child.stderr) {
			child.stderr.on('data', (chunk: unknown) => {
				for (const line of String(chunk).split('\n')) {
					if (line !== '') stderrTail.push(line);
					if (stderrTail.length > MAX_STDERR_LINES) stderrTail.shift();
				}
			});
		}
		child.once('exit', (code: number | null) => {
			exitCode = code;
		});

		const closed = new Promise<void>((resolve) => {
			child.once('close', () => resolve());
			// A spawn failure ends the wait too; the empty stdout becomes a parse error.
			child.once('error', () => resolve());
		});
		if (timeoutMs > 0) {
			const cancel = new AbortController();
			// `status` answers in tens of milliseconds, so hitting this limit already means the
			// engine is wedged: kill the child (SIGKILL, it is not reasoning any more) and move on.
			const outcome = await Promise.race([
				closed.then(() => 'closed' as const),
				// Cancelled once the child closes, so a finished call leaves no timer behind.
				delay(timeoutMs, undefined, { signal: cancel.signal }).then(() => 'timeout' as const, () => 'closed' as const),
			]);
			cancel.abort();
			if (outcome === 'timeout') {
				child.kill('SIGKILL');
				await closed;
			}
		} else {
			await closed;
		}
		return { stdout, stderrTail, exitCode };
	}

	// --- status for the popup ---------------------------------------------------

	// One in-flight status read. The popup polls every few seconds; sharing the promise means a
	// slow machine never piles up `status` processes against the database.
	private statusPromise: Promise<StatusResult> | null = null;

	status(): Promise<StatusResult> {
		if (this.statusPromise !== null) return this.statusPromise;
		const done = this.readStatus().finally(() => {
			// The next poll starts a fresh spawn.
			this.statusPromise = null;
		});
		this.statusPromise = done;
		return done;
	}

	private async readStatus(): Promise<StatusResult> {
		const collected = await this.collect(['status', '--json'], this.statusTimeoutMs);
		const parsed = parseStatusReport(collected.stdout);
		if ('report' in parsed) return { kind: 'status', report: parsed.report };
		// An empty stdout with a non-zero exit is the engine refusing to answer: its own first
		// stderr line is the reason (hostile data, so it is only ever written as text).
		const first = collected.stderrTail[0] ?? '';
		const message =
			collected.stdout.trim() === '' && collected.exitCode !== null && collected.exitCode !== 0 && first !== ''
				? first
				: parsed.error;
		return { kind: 'error', message };
	}
}

type EnginePathRef = { file: string; baseArgs: string[] };
type SpawnLike = (file: string, args: readonly string[], options?: { stdio?: unknown }) => EngineProcess;

// The real spawner, used by the main process. `stdio: ['pipe','pipe','pipe']` keeps stdin an open pipe
// (the engine exits when it closes) and pipes stdout/stderr into the ring buffer.
export const defaultSpawn: SpawnLike = (file, args, options) =>
	nodeSpawn(file, [...args], (options as SpawnOptions) ?? { stdio: ['pipe', 'pipe', 'pipe'] }) as unknown as EngineProcess;
