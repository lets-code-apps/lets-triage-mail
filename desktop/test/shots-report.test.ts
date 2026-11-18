// Unit tests for the shots report: the pass/fail decision and the failure path (a rendering rejection is
// recorded and turns the job red) that review round 1 asked to be covered. No Electron, no GUI.
import assert from 'node:assert/strict';
import test from 'node:test';

import { finalize, exitCodeFor } from './shots/report.ts';

test('a clean pass (all OK, no violations, no error) is green', () => {
	const report = finalize({
		rows: [
			{ name: 'accounts-1100-light', status: 'OK', px: 0, ratio: 0 },
			{ name: 'settings-360-dark', status: 'OK', px: 0, ratio: 0 },
		],
		violations: [],
	});
	assert.equal(report.ok, true);
	assert.equal(report.error, null);
	assert.equal(exitCodeFor(report), 0);
});

test('a shot that differs from the baseline turns the report red', () => {
	const report = finalize({
		rows: [{ name: 'settings-1100-light', status: 'CHANGED', px: 835159, ratio: 0.999 }],
		violations: [],
	});
	assert.equal(report.ok, false);
	assert.equal(exitCodeFor(report), 1);
});

test('a missing baseline turns the report red', () => {
	const report = finalize({ rows: [{ name: 'accounts-1100-light', status: 'MISSING', px: 0, ratio: 0 }], violations: [] });
	assert.equal(exitCodeFor(report), 1);
});

test('a layout violation with otherwise-clean shots turns the report red', () => {
	const report = finalize({
		rows: [{ name: 'accounts-1100-light', status: 'OK', px: 0, ratio: 0 }],
		violations: ['accounts-1100-light: title not centred: centerX 572 vs W/2 550'],
	});
	assert.equal(report.ok, false);
	assert.equal(exitCodeFor(report), 1);
});

test('THE FAILURE PATH: a rejected render is recorded in the report and turns it red', () => {
	// What the review flagged: a timed-out load / rejected capture must still produce a report and a
	// non-zero exit rather than escape and skip the write + exit.
	const report = finalize({ rows: [], violations: [], error: 'loadFile accounts-1100-light timed out after 15000 ms' });
	assert.equal(report.error, 'loadFile accounts-1100-light timed out after 15000 ms');
	assert.equal(report.ok, false);
	assert.equal(exitCodeFor(report), 1);
});

test('a per-state render failure keeps the shot listed and turns the report red', () => {
	const report = finalize({
		rows: [
			{ name: 'accounts-1100-light', status: 'RENDER FAILED', px: 0, ratio: 0 },
			{ name: 'accounts-1100-dark', status: 'OK', px: 0, ratio: 0 },
		],
		violations: [],
		error: 'capturePage settings-1100-dark timed out after 10000 ms',
	});
	assert.equal(report.ok, false);
	assert.equal(exitCodeFor(report), 1);
	// The failing shot is still in the rows, so the step summary can list it.
	assert.ok(report.rows.some((r) => r.status === 'RENDER FAILED'));
});
