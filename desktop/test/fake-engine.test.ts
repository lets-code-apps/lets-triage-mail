// The smoke test drives these fixtures through the desktop, so the fake engine's own status modes need
// a direct check: `--calm` must actually report needs reply 0 (it silently reused the normal counts once),
// and `--paused` must report a stopped engine. Synthetic only — the fake prints fixtures, never mail.
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { join } from 'node:path';
import { test } from 'node:test';

const FAKE = join(import.meta.dirname, 'fixtures', 'fake-engine.mjs');

// Run the fake engine as a child and parse its stdout. A non-zero exit makes execFileSync throw, which is
// the right failure here: every mode below must answer.
const status = (argv: string[], env: Record<string, string> = {}): Record<string, unknown> => {
	const out = execFileSync(process.execPath, [FAKE, 'status', ...argv], { encoding: 'utf8', env: { ...process.env, ...env } });
	return JSON.parse(out) as Record<string, unknown>;
};

const countsOf = (report: Record<string, unknown>): Record<string, number> =>
	report.counts as Record<string, number>;

test('status --json reports the normal day (needs reply 4, running)', () => {
	const report = status([]);
	assert.equal(countsOf(report).to_look_at, 4);
	assert.equal((report.engine as Record<string, unknown>).running, true);
});

test('status --calm reports needs reply 0 everywhere', () => {
	const report = status(['--calm']);
	assert.equal(countsOf(report).to_look_at, 0);
	assert.equal((report.engine as Record<string, unknown>).running, true);
});

test('status --paused reports a stopped engine with the normal day counts', () => {
	const report = status(['--paused']);
	const engine = report.engine as Record<string, unknown>;
	assert.equal(engine.running, false);
	assert.equal(engine.started_at, null);
	assert.equal(countsOf(report).to_look_at, 4);
});

test('the LTM_FAKE_STATUS env selects the same modes the desktop uses for the smoke', () => {
	// The desktop always runs `status --json`; the smoke asks for a state with LTM_FAKE_STATUS.
	assert.equal(countsOf(status([], { LTM_FAKE_STATUS: 'calm' })).to_look_at, 0);
	assert.equal((status([], { LTM_FAKE_STATUS: 'paused' }).engine as Record<string, unknown>).running, false);
});
