import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { parseCheckReport } from '../src/check-report.ts';

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
