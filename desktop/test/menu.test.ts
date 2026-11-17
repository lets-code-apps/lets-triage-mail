import assert from 'node:assert/strict';
import { test } from 'node:test';

import { buildTrayMenu, statusText } from '../src/menu.ts';
import type { EngineState } from '../src/ipc.ts';

const STATES: EngineState[] = ['stopped', 'starting', 'running', 'stopping', 'failed'];

const input = (state: EngineState, over: Partial<import('../src/menu.ts').TrayMenuInput> = {}) => ({
	state,
	version: '0.1.0',
	exitCode: null,
	signal: null,
	...over,
});

const byLabel = (state: EngineState, label: string) =>
	buildTrayMenu(input(state)).find((item) => item.label === label);

test('Start triage is enabled only when stopped or failed', () => {
	for (const state of STATES) {
		const item = byLabel(state, 'Start triage');
		assert.equal(item?.enabled, state === 'stopped' || state === 'failed');
	}
});

test('Stop triage is enabled only when running', () => {
	for (const state of STATES) {
		const item = byLabel(state, 'Stop triage');
		assert.equal(item?.enabled, state === 'running');
	}
});

test('status text is stable per state', () => {
	const expected: Record<EngineState, string> = {
		stopped: 'Stopped',
		starting: 'Starting…',
		running: 'Running',
		stopping: 'Stopping…',
		failed: 'Failed',
	};
	for (const state of STATES) {
		assert.equal(statusText(input(state)), expected[state]);
		// The status line is the second item, and it mirrors statusText.
		assert.equal(buildTrayMenu(input(state))[1].label, expected[state]);
	}
});

test('Quit is always present with the Cmd+Q accelerator', () => {
	for (const state of STATES) {
		const quit = buildTrayMenu(input(state)).find((item) => item.label === "Quit Let's Triage Mail");
		assert.equal(quit?.accelerator, 'Cmd+Q');
		assert.equal(quit?.enabled, true);
	}
});

test('a failure shows the exit code or the signal', () => {
	assert.equal(statusText(input('failed', { exitCode: 1 })), 'Failed (exit 1)');
	assert.equal(statusText(input('failed', { signal: 'SIGKILL' })), 'Failed (SIGKILL)');
});

test('the version appears in its own disabled item', () => {
	const items = buildTrayMenu(input('running', { version: '0.9.9' }));
	const item = items.find((i) => i.label === 'Engine 0.9.9');
	assert.equal(item?.enabled, false);
});
