import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { isKnownState, parseStatusReport } from '../src/status-report.ts';

const FIXTURE = join(import.meta.dirname, 'fixtures', 'status-report.json');

const fixtureObject = (): Record<string, unknown> =>
	JSON.parse(readFileSync(FIXTURE, 'utf8')) as Record<string, unknown>;

test('parses the fixture report', () => {
	const result = parseStatusReport(readFileSync(FIXTURE, 'utf8'));
	assert.ok('report' in result, JSON.stringify(result));
	const report = result.report;
	assert.equal(report.version, 1);
	assert.equal(report.engine.running, true);
	assert.equal(report.engine.started_at, 1794000000);
	assert.equal(report.window.kind, 'today');
	assert.equal(report.counts.to_look_at, 4);
	assert.equal(report.accounts.length, 2);
	assert.equal(report.accounts[1].mode, 'dry-run');
	assert.equal(report.accounts[1].state, 'sign_in_failed');
});

test('keeps engine timestamps as null when it is not running', () => {
	const raw = fixtureObject();
	const engine = raw.engine as Record<string, unknown>;
	engine.running = false;
	engine.started_at = null;
	engine.heartbeat_at = null;
	const result = parseStatusReport(JSON.stringify(raw));
	assert.ok('report' in result, JSON.stringify(result));
	assert.equal(result.report.engine.running, false);
	assert.equal(result.report.engine.started_at, null);
});

test('ignores extra keys the engine may add later', () => {
	const raw = fixtureObject();
	raw.future_field = 'whatever';
	const result = parseStatusReport(JSON.stringify(raw));
	assert.ok('report' in result, JSON.stringify(result));
});

test('tolerates a state string this app has never seen', () => {
	const raw = fixtureObject();
	const account = (raw.accounts as Record<string, unknown>[])[0];
	account.state = 'warming_up';
	const result = parseStatusReport(JSON.stringify(raw));
	assert.ok('report' in result, JSON.stringify(result));
	assert.equal(result.report.accounts[0].state, 'warming_up');
	assert.equal(isKnownState(result.report.accounts[0].state), false);
});

test('rejects output that is not JSON', () => {
	const result = parseStatusReport('panic: could not open the database');
	assert.ok('error' in result);
	assert.match(result.error, /valid JSON/);
});

test('rejects empty stdout', () => {
	const result = parseStatusReport('   ');
	assert.ok('error' in result);
	assert.match(result.error, /no output/);
});

test('rejects a shape version this app does not speak', () => {
	const raw = fixtureObject();
	raw.version = 2;
	const result = parseStatusReport(JSON.stringify(raw));
	assert.ok('error' in result);
	assert.match(result.error, /unsupported version/);
});

test('rejects a missing accounts array', () => {
	const raw = fixtureObject();
	delete raw.accounts;
	const result = parseStatusReport(JSON.stringify(raw));
	assert.ok('error' in result);
	assert.match(result.error, /accounts/);
});

test('rejects ill-typed fields rather than guessing', () => {
	const cases: [string, RegExp][] = [
		['{"version":1}', /generated_at/],
		[JSON.stringify({ ...fixtureObject(), engine: 'up' }), /engine/],
		[JSON.stringify({ ...fixtureObject(), window: { kind: 'today' } }), /since/],
		[JSON.stringify({ ...fixtureObject(), counts: { to_look_at: 'lots' } }), /totals/],
		[JSON.stringify({ ...fixtureObject(), engine: { running: true, started_at: 'yesterday', heartbeat_at: null } }), /timestamps/],
	];
	for (const [body, pattern] of cases) {
		const result = parseStatusReport(body);
		assert.ok('error' in result, body);
		assert.match(result.error, pattern, body);
	}
});

test('all six states the engine writes are known here', () => {
	for (const state of ['connecting', 'connected', 'reconnecting', 'sign_in_failed', 'failed', 'not_running']) {
		assert.ok(isKnownState(state), state);
	}
});
