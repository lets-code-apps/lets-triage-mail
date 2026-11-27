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

// --- the first-run onboarding commands -----------------------------------------------------------
// Setup reads the config and the model situation, then writes them, so the fixture must answer those
// commands with the real engine's shapes. These checks are what keep the smoke test's first-run path honest.

// Run the fixture and keep stdout even when it exits non-zero (a refused command still has to answer).
function run(argv: string[], env: Record<string, string> = {}, input?: string): { stdout: string; code: number } {
	try {
		const stdout = execFileSync(process.execPath, [FAKE, ...argv], { encoding: 'utf8', env: { ...process.env, ...env }, input });
		return { stdout, code: 0 };
	} catch (thrown) {
		const failure = thrown as { stdout?: string; status?: number };
		return { stdout: failure.stdout ?? '', code: failure.status ?? 1 };
	}
}

const jsonOf = (argv: string[], env: Record<string, string> = {}): Record<string, unknown> =>
	JSON.parse(run(argv, env).stdout) as Record<string, unknown>;

test('config show --json answers the three first-run states', () => {
	const withAccount = jsonOf(['config', 'show', '--json']);
	assert.equal(withAccount.ok, true);
	assert.equal((withAccount.accounts as unknown[]).length, 1);
	assert.equal((withAccount.digest as Record<string, unknown>).enabled, true);

	const empty = jsonOf(['config', 'show', '--json'], { LTM_FAKE_CONFIG: 'empty' });
	assert.equal(empty.ok, true);
	assert.equal((empty.accounts as unknown[]).length, 0, 'an empty config is what opens setup');

	const missing = run(['config', 'show', '--json'], { LTM_FAKE_CONFIG: 'missing' });
	assert.equal(missing.code, 1, 'no file is a failure the way the real engine reports it');
	const failed = JSON.parse(missing.stdout) as Record<string, unknown>;
	assert.equal(failed.ok, false);
	assert.equal(failed.missing, true, 'the desktop needs missing:true to open setup');
});

test('config set and an account write answer restart_needed, and never echo the password', () => {
	const set = jsonOf(['config', 'set', 'digest.enabled', 'false', '--json']);
	assert.equal(set.ok, true);
	assert.equal(set.restart_needed, true);

	const added = run(['config', 'account', 'add', '--json', '--name=Example', '--host=imap.example.com'], {}, 'hunter2\n');
	assert.equal((JSON.parse(added.stdout) as Record<string, unknown>).ok, true);
	assert.ok(!added.stdout.includes('hunter2'), 'a password must never come back on stdout');
});

test('model detect --json answers shared, found, found-empty and not-found', () => {
	const found = jsonOf(['model', 'detect', '--json']);
	assert.equal(found.ok, true);
	const first = (found.runtimes as Record<string, unknown>[])[0];
	assert.equal(first.reachable, true);
	assert.equal((first.usable as string[]).length, 2);
	assert.equal(found.shared, null);

	const shared = jsonOf(['model', 'detect', '--json'], { LTM_FAKE_MODEL: 'shared' });
	assert.equal((shared.shared as Record<string, unknown>).set_by, 'rag docs');

	const empty = jsonOf(['model', 'detect', '--json'], { LTM_FAKE_MODEL: 'found-empty' });
	assert.equal(((empty.runtimes as Record<string, unknown>[])[0].usable as string[]).length, 0, 'a server with no chat model');

	const none = jsonOf(['model', 'detect', '--json'], { LTM_FAKE_MODEL: 'not-found' });
	const probe = (none.runtimes as Record<string, unknown>[])[0];
	assert.equal(probe.reachable, false);
	assert.equal(typeof probe.error, 'string');
});

test('model pull --json streams normalised lines in the engine\'s shape', () => {
	const out = run(['model', 'pull', '--json', '--', 'qwen3:8b']);
	assert.equal(out.code, 0);
	const lines = out.stdout.split('\n').filter((line) => line !== '').map((line) => JSON.parse(line) as Record<string, unknown>);
	assert.equal(lines[0].stage, 'pulling manifest');
	assert.equal(lines[lines.length - 1].stage, 'success');
	const downloading = lines.filter((line) => line.stage === 'downloading');
	assert.ok(downloading.length >= 2, 'the bar needs progress, not one jump');
	for (const line of downloading) {
		const total = line.total as number;
		const completed = line.completed as number;
		assert.ok(total > 5_000_000_000 && total < 6_000_000_000, 'the design sizes qwen3:8b at 5.2 GB');
		assert.ok(completed <= total && completed >= 0);
	}
	assert.ok((downloading[0].completed as number) < (downloading[downloading.length - 1].completed as number), 'it must go up');
});

test('a failed pull ends with an error line and a non-zero exit', () => {
	const out = run(['model', 'pull', '--json', '--', 'qwen3:8b'], { LTM_FAKE_MODEL: 'pull-fail' });
	assert.equal(out.code, 1);
	const lines = out.stdout.split('\n').filter((line) => line !== '').map((line) => JSON.parse(line) as Record<string, unknown>);
	const last = lines[lines.length - 1];
	assert.equal(last.stage, 'error');
	assert.equal(typeof last.error, 'string');
});

test('model save --json answers the runtime, url and model it was told', () => {
	const saved = jsonOf(['model', 'save', '--json', '--runtime=ollama', '--url=http://localhost:11434', '--model=qwen3:8b']);
	assert.equal(saved.ok, true);
	assert.equal(saved.runtime, 'ollama');
	assert.equal(saved.url, 'http://localhost:11434');
	assert.equal(saved.model, 'qwen3:8b');
});
