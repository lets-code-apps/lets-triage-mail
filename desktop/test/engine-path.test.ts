import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { test } from 'node:test';

import { resolveEngine } from '../src/engine-path.ts';

test('LTM_ENGINE_PATH wins over every other lookup', () => {
	const r = resolveEngine({ LTM_ENGINE_PATH: '/opt/ltm' }, undefined, '/repo');
	assert.equal(r.file, '/opt/ltm');
});

test('LTM_CONFIG is forwarded as a global --config', () => {
	const r = resolveEngine({ LTM_ENGINE_PATH: '/opt/ltm', LTM_CONFIG: '/home/me/cfg' }, undefined, '/repo');
	assert.deepEqual(r.baseArgs, ['--config', '/home/me/cfg']);
});

test('no LTM_CONFIG means no --config (the engine finds the default files)', () => {
	const r = resolveEngine({ LTM_ENGINE_PATH: '/opt/ltm' }, undefined, '/repo');
	assert.deepEqual(r.baseArgs, []);
});

test('the packaged path is preferred over the debug path', () => {
	const root = mkdtempSync(join(tmpdir(), 'ltm-path-'));
	const resources = join(root, 'res');
	mkdirSync(join(resources, 'engine'), { recursive: true });
	writeFileSync(join(resources, 'engine', 'lets-triage-mail'), '');
	const repo = join(root, 'repo');
	mkdirSync(join(repo, 'target', 'debug'), { recursive: true });
	writeFileSync(join(repo, 'target', 'debug', 'lets-triage-mail'), '');

	const r = resolveEngine({}, resources, repo);
	assert.equal(r.file, join(resources, 'engine', 'lets-triage-mail'));
});

test('falls back to the debug build', () => {
	const root = mkdtempSync(join(tmpdir(), 'ltm-path-'));
	const repo = join(root, 'repo');
	mkdirSync(join(repo, 'target', 'debug'), { recursive: true });
	writeFileSync(join(repo, 'target', 'debug', 'lets-triage-mail'), '');

	const r = resolveEngine({}, join(root, 'no-resources'), repo);
	assert.equal(r.file, join(repo, 'target', 'debug', 'lets-triage-mail'));
});

test('no binary at all gives an actionable error', () => {
	const root = mkdtempSync(join(tmpdir(), 'ltm-path-'));
	assert.throws(() => resolveEngine({}, undefined, root), /cargo build/);
});
