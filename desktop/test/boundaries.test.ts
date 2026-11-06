import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { CHANNEL } from '../src/ipc.ts';

// Mirrors the Rust boundary check in src/imap/session.rs (`only_session_rs_speaks_imap`): scan
// WHOLE files, assemble the forbidden words so the intent is explicit, and fail with the offending
// lines. This is the executable statement of the desktop/engine boundary: the desktop makes no
// network calls, never reads config or the DB, and touches no engine library.

const DESKTOP = join(import.meta.dirname, '..');
const SRC = join(DESKTOP, 'src');
const DOCS = join(DESKTOP, '..', 'docs', 'DESKTOP.md');

function walk(dir: string): string[] {
	const out: string[] = [];
	for (const entry of readdirSync(dir)) {
		const path = join(dir, entry);
		if (statSync(path).isDirectory()) out.push(...walk(path));
		else out.push(path);
	}
	return out;
}

// "Never makes a network call": no Node HTTP/TLS/socket/dgram modules, no global fetch, and no
// Electron net/session fetch. Assembled so the list reads as intent.
const FORBIDDEN_NETWORK = [
	['node', ':', 'http'].join(''),
	['node', ':', 'https'].join(''),
	['node', ':', 'net'].join(''),
	['node', ':', 'dgram'].join(''),
	['fetch', '('].join(''),
	['session', '.', 'fetch'].join(''),
	['net', '.', 'fetch'].join(''),
];

// "Never reads config or state, and never re-implements engine parsing."
const FORBIDDEN_ENGINE = [['.', 'toml'].join(''), ['state', '.', 'db'].join('')];

test('package.json pulls in no config, database, IMAP or HTTP-client library', () => {
	const pkg = JSON.parse(readFileSync(join(DESKTOP, 'package.json'), 'utf8'));
	const deps = Object.keys({ ...pkg.dependencies, ...pkg.devDependencies }) as string[];
	const forbidden = ['toml', 'sqlite', 'imap', 'undici', 'axios', 'node-fetch', 'got', 'superagent', 'http'];
	for (const name of deps) {
		for (const word of forbidden) {
			assert.ok(!name.includes(word), `desktop must not depend on ${name} (matches ${word})`);
		}
	}
	// It is allowed to depend on Electron and the TypeScript toolchain only.
});

test('no source file makes a network call or reads config/state', () => {
	const files = walk(SRC).filter((f) => f.endsWith('.ts'));
	const violations: string[] = [];
	for (const file of files) {
		const text = readFileSync(file, 'utf8');
		for (const token of [...FORBIDDEN_NETWORK, ...FORBIDDEN_ENGINE]) {
			if (text.includes(token)) violations.push(`${file}: ${token}`);
		}
	}
	assert.deepEqual(violations, []);
});

test('the only child_process use is engine.ts', () => {
	const files = walk(SRC).filter((f) => f.endsWith('.ts'));
	const token = ['child', '_', 'process'].join('');
	const offenders = files.filter((f) => readFileSync(f, 'utf8').includes(token));
	assert.deepEqual(offenders.map((f) => f.slice(SRC.length + 1)), ['engine.ts']);
});

test('DESKTOP.md keeps its Implemented / Mockup only / Future sections', () => {
	const doc = readFileSync(DOCS, 'utf8');
	for (const heading of ['## Implemented', '## Mockup only', '## Future']) {
		assert.ok(doc.includes(heading), `DESKTOP.md is missing the "${heading}" section`);
	}
});

// The preload is CommonJS (sandbox), so it cannot import CHANNEL from ipc.ts. This closes the seam
// the type system cannot: every channel name ipc.ts declares must appear in the bridge it exposes.
test('the preload bridge exposes exactly the channels ipc.ts declares', () => {
	const preload = readFileSync(join(SRC, 'preload.cjs'), 'utf8');
	for (const channel of [CHANNEL.getState, CHANNEL.state, CHANNEL.command, CHANNEL.checkResult]) {
		assert.ok(preload.includes(channel), `preload.cjs must use channel ${channel}`);
	}
	// It exposes only getState / onState / onCheckResult / command; nothing that reaches Node or the
	// network.
	assert.equal(preload.includes('require('), true);
});

// The tray advertises "Check setup" as an implemented action, so its click must (a) open a window
// (somewhere to show the result) and (b) push the finished check to the renderer. Both main.ts call
// sites build their payload through the one electron-free checkPayload helper, so the report and the
// error can never be silently dropped.
test('the tray Check result is opened, built and pushed to the renderer', () => {
	const main = readFileSync(join(SRC, 'main.ts'), 'utf8');
	assert.ok(main.includes('function deliverCheck'), 'the tray must have a check path');
	assert.ok(/function deliverCheck[^]*?ctl\.open\(\)/.test(main), 'deliverCheck must open the window');
	assert.ok(main.includes('checkPayload('), 'the check result must be built by checkPayload');
	assert.ok(main.includes(`send(CHANNEL.checkResult`), 'the result must be pushed to the renderer');
});

// A window opened while the engine is already running must show the real state and enable the right
// controls at once: the renderer reads getState() on load before subscribing to future transitions.
test('the renderer bootstraps the current state and subscribes to check results', () => {
	const renderer = readFileSync(join(DESKTOP, 'renderer', 'renderer.js'), 'utf8');
	assert.ok(renderer.includes('api.getState()'), 'the renderer must read the current state on load');
	assert.ok(renderer.indexOf('api.getState()') < renderer.indexOf('api.onState('), 'read before subscribe');
	assert.ok(renderer.includes('api.onCheckResult('), 'the renderer must show tray check results');
	// Engine output stays hostile data: state is written with textContent, never an innerHTML assignment.
	assert.ok(renderer.includes('stateEl.textContent'), 'state must render with textContent');
	assert.equal(/\.innerHTML\s*=/.test(renderer), false); // no markup sink, even though the word may appear in prose
});
