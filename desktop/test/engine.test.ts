import assert from 'node:assert/strict';
import { EventEmitter } from 'node:events';
import { PassThrough } from 'node:stream';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { EngineController, type EngineProcess } from '../src/engine.ts';

// The engine is driven with a fake child so no real process, signal or timer is involved: `spawn`
// and the stop timeout are injected. This mirrors how the Rust supervisor tests inject a closure.
const REPORT = readFileSync(join(import.meta.dirname, 'fixtures', 'check-report.json'), 'utf8');
// A fixed env so resolveEngine never hits the filesystem: an explicit path short-circuits the lookup.
const ENV: NodeJS.ProcessEnv = { LTM_ENGINE_PATH: '/fake/lets-triage-mail' };

type LongChild = { child: EngineProcess; stdout: PassThrough; stderr: PassThrough; signals: string[] };

function makeLong(runKind: 'normal' | 'ignore'): LongChild {
	const signals: string[] = [];
	const stdout = new PassThrough();
	const stderr = new PassThrough();
	const child = new EventEmitter() as unknown as EngineProcess;
	child.stdout = stdout;
	child.stderr = stderr;
	child.kill = (signal?: NodeJS.Signals | number) => {
		signals.push(String(signal));
		if (signal === 'SIGKILL') child.emit('exit', null, 'SIGKILL');
		else if (signal === 'SIGTERM' && runKind === 'normal') child.emit('exit', 0, null);
	};
	return { child, stdout, stderr, signals };
}

function makeSpawn() {
	const state = { long: [] as LongChild[], runKind: 'normal' as 'normal' | 'ignore', checkMode: 'json' as 'json' | 'nonjson' };
	const spawn = (file: string, args: readonly string[], options?: { stdio?: unknown }) => {
		void file;
		void options;
		if (args.includes('run')) {
			const long = makeLong(state.runKind);
			state.long.push(long);
			return long.child;
		}
		// A short-lived one-shot child (check / --version): it writes then closes.
		const one = new EventEmitter() as unknown as EngineProcess;
		const stdout = new PassThrough();
		const stderr = new PassThrough();
		one.stdout = stdout;
		one.stderr = stderr;
		one.kill = () => {};
		const wantVersion = args.includes('--version');
		queueMicrotask(() => {
			if (wantVersion) stdout.write('lets-triage-mail 0.1.0\n');
			else if (state.checkMode === 'json') stdout.write(REPORT);
			else {
				stdout.write('boom, not json\n');
				stderr.write('server unreachable\n');
			}
			one.emit('close');
		});
		return one;
	};
	return { spawn, state };
}

const controller = (spawn: ReturnType<typeof makeSpawn>, stopTimeoutMs = 5) =>
	new EngineController(ENV, '/unused', spawn.spawn, stopTimeoutMs);

test('start walks stopped -> starting -> running on the spawn event', () => {
	const s = makeSpawn();
	const e = controller(s);
	assert.equal(e.getState().state, 'stopped');
	e.start();
	assert.equal(e.getState().state, 'starting');
	s.state.long[0].child.emit('spawn');
	assert.equal(e.getState().state, 'running');
});

test('start while running is a no-op', () => {
	const s = makeSpawn();
	const e = controller(s);
	e.start();
	s.state.long[0].child.emit('spawn');
	e.start();
	assert.equal(s.state.long.length, 1);
});

test('the ring buffer keeps the last 500 log lines', () => {
	const s = makeSpawn();
	const e = controller(s);
	e.start();
	for (let i = 0; i < 600; i += 1) s.state.long[0].stdout.write(`log line ${i}\n`);
	assert.equal(e.logCount(), 500);
});

test('an unexpected exit code moves the engine to failed, with no restart', () => {
	const s = makeSpawn();
	const e = controller(s);
	e.start();
	const c = s.state.long[0];
	c.stderr.write('engine: every account failed\n');
	c.child.emit('exit', 1, null);
	assert.equal(e.getState().state, 'failed');
	assert.equal(e.getState().exitCode, 1);
	assert.equal(s.state.long.length, 1);
});

test('failed carries the last stderr lines; a fresh start clears the tail', () => {
	const s = makeSpawn();
	const e = controller(s);
	e.start();
	s.state.long[0].child.emit('spawn');
	assert.deepEqual(e.getState().failureTail, []); // running has no failure tail
	for (let i = 0; i < 12; i += 1) s.state.long[0].stderr.write(`boom ${i}\n`);
	s.state.long[0].child.emit('exit', 1, null);
	const tail = e.getState().failureTail;
	assert.equal(tail.length, 8); // bounded to MAX_STDERR_LINES
	assert.equal(tail[7], 'boom 11'); // the most recent line is kept
	e.start();
	assert.deepEqual(e.getState().failureTail, []); // a fresh attempt starts with an empty tail
});

test('stop sends exactly one SIGTERM and lands on stopped on a clean exit', async () => {
	const s = makeSpawn();
	const e = controller(s);
	e.start();
	s.state.long[0].child.emit('spawn');
	await e.stop();
	assert.deepEqual(s.state.long[0].signals, ['SIGTERM']);
	assert.equal(e.getState().state, 'stopped');
});

test('a second stop while stopping reuses the in-flight stop and sends one SIGTERM', async () => {
	// Stop-then-Quit: two stop() calls arrive while the first is still waiting on a slow engine.
	// A second SIGTERM would make the Rust engine exit 130 and skip cleanup, so it must not happen.
	const s = makeSpawn();
	s.state.runKind = 'ignore'; // engine ignores SIGTERM, so the first stop is genuinely in flight
	const e = controller(s);
	e.start();
	s.state.long[0].child.emit('spawn');
	const first = e.stop();
	const second = e.stop();
	assert.equal(first, second); // same in-flight promise, not a new signal
	await Promise.all([first, second]);
	assert.deepEqual(s.state.long[0].signals, ['SIGTERM', 'SIGKILL']);
	assert.equal(s.state.long[0].signals.filter((x) => x === 'SIGTERM').length, 1);
	assert.equal(e.getState().state, 'stopped');
});

test('stop escalates to SIGKILL when the engine ignores SIGTERM', async () => {
	const s = makeSpawn();
	s.state.runKind = 'ignore';
	const e = controller(s);
	e.start();
	s.state.long[0].child.emit('spawn');
	await e.stop();
	assert.deepEqual(s.state.long[0].signals, ['SIGTERM', 'SIGKILL']);
	assert.equal(e.getState().state, 'stopped');
});

test('stop during starting works; stop when already stopped is a no-op', async () => {
	const s = makeSpawn();
	const starting = controller(s);
	starting.start(); // never spawned; still stoppable
	await starting.stop();
	assert.deepEqual(s.state.long[0].signals, ['SIGTERM']);
	assert.equal(starting.getState().state, 'stopped');

	const s2 = makeSpawn();
	const stopped = controller(s2);
	await stopped.stop();
	assert.equal(s2.state.long.length, 0);
	assert.equal(stopped.getState().state, 'stopped');
});

test('runCheck returns items for exit 0 AND exit 1 (exit code is ignored)', async () => {
	const s = makeSpawn();
	const e = controller(s);
	const first = await e.runCheck();
	const second = await e.runCheck();
	assert.equal(first.kind, 'report');
	assert.equal(second.kind, 'report');
	if (first.kind === 'report') assert.equal(first.report.items.length, 3);
});

test('non-JSON check output becomes an error carrying the stderr tail', async () => {
	const s = makeSpawn();
	s.state.checkMode = 'nonjson';
	const e = controller(s);
	const result = await e.runCheck();
	assert.equal(result.kind, 'error');
	if (result.kind === 'error') assert.match(result.message, /server unreachable/);
});

test('readVersion strips the binary name down to the version token', async () => {
	const s = makeSpawn();
	const e = controller(s);
	assert.equal(await e.readVersion(), '0.1.0');
});
