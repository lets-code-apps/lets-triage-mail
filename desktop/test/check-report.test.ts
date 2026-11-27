import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { modelAnswer, parseCheckReport } from '../src/check-report.ts';

const FIXTURE = join(import.meta.dirname, 'fixtures', 'check-report.json');

test('parses the fixture report', () => {
	const result = parseCheckReport(readFileSync(FIXTURE, 'utf8'));
	assert.ok('report' in result);
	const report = result.report;
	assert.equal(report.ok, false);
	assert.equal(report.failed, 1);
	assert.equal(report.warnings, 1);
	assert.equal(report.items.length, 3);
	assert.equal(report.items[0].status, 'ok');
	assert.equal(report.items[1].status, 'warn');
	assert.equal(report.items[2].status, 'fail');
});

test('accepts hint: null and keeps a string hint', () => {
	const result = parseCheckReport(readFileSync(FIXTURE, 'utf8'));
	if ('report' in result) {
		assert.equal(result.report.items[0].hint, null);
		assert.equal(typeof result.report.items[2].hint, 'string');
	} else {
		assert.fail('expected a report');
	}
});

test('passes engine text through unchanged (no re-wording)', () => {
	const result = parseCheckReport(readFileSync(FIXTURE, 'utf8'));
	if ('report' in result) {
		assert.equal(result.report.items[2].message, 'sign-in failed on imap.example.com: connection refused');
	} else {
		assert.fail('expected a report');
	}
});

test('rejects output with no items array', () => {
	const result = parseCheckReport('{"ok":true,"failed":0,"warnings":0}');
	assert.ok('error' in result);
	assert.match(result.error, /items/);
});

test('rejects a non-array items value', () => {
	const result = parseCheckReport('{"ok":true,"failed":0,"warnings":0,"items":"nope"}');
	assert.ok('error' in result);
	assert.match(result.error, /items/);
});

test('rejects an unknown status', () => {
	const result = parseCheckReport('{"ok":true,"failed":0,"warnings":0,"items":[{"area":"a","status":"maybe","message":"m","hint":null}]}');
	assert.ok('error' in result);
	assert.match(result.error, /unknown status/);
});

test('rejects a non-JSON stdout', () => {
	const result = parseCheckReport('boom, not json at all');
	assert.ok('error' in result);
	assert.match(result.error, /valid JSON/);
});

// The onboarding model step's "ready" is decided from the model part of the report (see setup-model.ts).
const MODEL_REPORT = JSON.stringify({
	ok: true,
	failed: 0,
	warnings: 0,
	items: [
		{ area: 'Config', status: 'ok', message: 'loads', hint: null },
		{ area: 'Model', status: 'ok', message: 'model server answers at http://localhost:11434', hint: null },
		{ area: 'Model', status: 'ok', message: 'model `qwen3:8b` is available', hint: null },
		{ area: 'Model', status: 'ok', message: 'test classification worked in 1.4 s (judged "junk" with confidence 0.91)', hint: null },
		{ area: 'Account a', status: 'ok', message: 'connected', hint: null },
	],
});

test('modelAnswer returns the engine test-classification line, verbatim', () => {
	const parsed = parseCheckReport(MODEL_REPORT);
	assert.ok('report' in parsed);
	assert.equal(modelAnswer(parsed.report), 'test classification worked in 1.4 s (judged "junk" with confidence 0.91)');
});

test('modelAnswer is null when the classification failed or is absent', () => {
	const failed = parseCheckReport(
		JSON.stringify({
			ok: false,
			failed: 1,
			warnings: 0,
			items: [
				{ area: 'Model', status: 'ok', message: 'model server answers at http://localhost:11434', hint: null },
				{ area: 'Model', status: 'fail', message: 'the test classification failed: the model did not return the expected JSON', hint: 'try a larger model' },
			],
		}),
	);
	assert.ok('report' in failed);
	assert.equal(modelAnswer(failed.report), null, 'a failed classification is no proof of an answer');

	const absent = parseCheckReport(
		JSON.stringify({ ok: true, failed: 0, warnings: 0, items: [{ area: 'Model', status: 'ok', message: 'model server answers at http://localhost:11434', hint: null }] }),
	);
	assert.ok('report' in absent);
	assert.equal(modelAnswer(absent.report), null, 'reachability alone is not an answer');
});
