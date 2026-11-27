import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { ConfigAccount } from '../src/account-edit.ts';
import { buildEditRequests } from '../src/account-edit.ts';
import {
	MODE_TO_FLAGGING,
	accountNameFor,
	buildAccountSetArgs,
	modeChanges,
	buildConfigSetArgs,
	buildModelDetectArgs,
	buildModelPullArgs,
	buildModelSaveArgs,
	decideSetup,
	guessServer,
	parseConfigGlobal,
	parseModelDetect,
	parsePullLine,
	pullProgress,
	progressBar,
	recommendedModel,
} from '../src/setup.ts';

const acct = (name: string): ConfigAccount => ({ name, host: 'h', port: 993, username: 'u', tls: 'tls', mode: '', passwordSource: 'command' });

test('decideSetup: first run opens setup; a second launch skips; existing accounts go silent; a broken file shows the normal screen', () => {
	// No config file at all -> setup.
	assert.equal(decideSetup(false, { ok: false, error: 'no config file at /x', missing: true }), 'setup');
	// A config file with no accounts -> setup.
	assert.equal(decideSetup(false, { ok: true, accounts: [] }), 'setup');
	// Accounts already exist but setup was never recorded -> silent (record setupDone, no window).
	assert.equal(decideSetup(false, { ok: true, accounts: [acct('work')] }), 'silent');
	// A broken config (not missing) -> skip; the normal screen shows the error, setup does not mask it.
	assert.equal(decideSetup(false, { ok: false, error: 'toml parse error', missing: false }), 'skip');
	// Onboarding already done -> skip.
	assert.equal(decideSetup(true, { ok: true, accounts: [acct('work')] }), 'skip');
});

test('guessServer maps the four big providers and falls back to imap.<domain>', () => {
	assert.deepEqual(guessServer('me@gmail.com'), { host: 'imap.gmail.com', port: 993 });
	assert.deepEqual(guessServer('me@GOOGLEmail.com'), { host: 'imap.gmail.com', port: 993 });
	assert.deepEqual(guessServer('x@ICLOUD.com'), { host: 'imap.mail.me.com', port: 993 });
	assert.deepEqual(guessServer('x@outlook.com'), { host: 'outlook.office365.com', port: 993 });
	assert.deepEqual(guessServer('x@fastmail.fm'), { host: 'imap.fastmail.com', port: 993 });
	assert.deepEqual(guessServer('x@corp.example.co.uk'), { host: 'imap.corp.example.co.uk', port: 993 });
});

test('accountNameFor swaps disallowed chars for -, starts on a letter/digit, truncates to 64, disambiguates a clash', () => {
	assert.equal(accountNameFor('ann+o@x.com', []), 'ann-o');
	assert.equal(accountNameFor('+ann@x.com', []), 'ann');
	const long = accountNameFor(`${'a'.repeat(80)}@x.com`, []);
	assert.equal(long, 'a'.repeat(64));
	assert.equal(accountNameFor('alice@x.com', ['alice']), 'alice 2');
	assert.equal(accountNameFor('alice@x.com', ['alice', 'alice 2']), 'alice 3');
});

test('recommendedModel sizes the model to the Mac', () => {
	const GB = 1024 ** 3;
	assert.deepEqual(recommendedModel(8 * GB), { model: 'qwen3:4b', sizeGb: 2.5 });
	assert.deepEqual(recommendedModel(12 * GB), { model: 'qwen3:8b', sizeGb: 5.2 });
	assert.deepEqual(recommendedModel(16 * GB), { model: 'qwen3:8b', sizeGb: 5.2 });
	assert.deepEqual(recommendedModel(24 * GB), { model: 'qwen3:8b', sizeGb: 5.2 });
	assert.deepEqual(recommendedModel(32 * GB), { model: 'qwen3:14b', sizeGb: 9.3 });
	assert.deepEqual(recommendedModel(64 * GB), { model: 'qwen3:14b', sizeGb: 9.3 });
});

test('buildConfigSetArgs allows only the three global keys, and never a value that could read as a flag', () => {
	assert.deepEqual(buildConfigSetArgs('policy.dry_run', 'true'), ['set', 'policy.dry_run', 'true', '--json']);
	assert.deepEqual(buildConfigSetArgs('digest.enabled', 'false'), ['set', 'digest.enabled', 'false', '--json']);
	assert.deepEqual(buildConfigSetArgs('digest.time', '08:00'), ['set', 'digest.time', '08:00', '--json']);
	assert.equal(buildConfigSetArgs('policy.bogus', 'true'), null, 'unknown key');
	assert.equal(buildConfigSetArgs('policy.dry_run', 'maybe'), null, 'bad value');
	assert.equal(buildConfigSetArgs('digest.time', '8am'), null, 'bad time');
	assert.equal(buildConfigSetArgs('--evil', 'x'), null, 'a flag is not a key');
	assert.equal(buildConfigSetArgs('digest.time', '--nope'), null, 'a value can never start with a dash');
});

test('a download bar keeps its maximum at 100 and the fill is the clamped percent, never a partial "full"', () => {
	// The bar is percent-based: at 42% the maximum stays 100 and the fill is 42%, so a screen reader and
	// the painted bar both read "42 of 100", not "42 of 42" complete.
	assert.deepEqual(progressBar(42), { valuemin: 0, valuemax: 100, valuenow: 42, fill: '42%' });
	assert.equal(progressBar(100).fill, '100%', 'a finished download fills the bar');
	assert.equal(progressBar(0).valuenow, 0, 'a download that has not started is empty');
	assert.equal(progressBar(140).valuenow, 100, 'a value over 100 is clamped');
	assert.equal(progressBar(-5).valuenow, 0, 'a negative value is clamped');
	assert.equal(progressBar(41.7).valuenow, 42, 'a fractional percent is rounded');
});

test('mode is the per-account flagging the engine writes, per the board decision', () => {
	// classify only = the per-account classify-only mode: flagging off, and live so the digest appends.
	assert.deepEqual(modeChanges('classify-only'), [{ key: 'flagging', value: 'false' }, { key: 'dry_run', value: 'false' }]);
	// classify and sort = flagging on and live.
	assert.deepEqual(modeChanges('classify-and-sort'), [{ key: 'flagging', value: 'true' }, { key: 'dry_run', value: 'false' }]);
	assert.equal(MODE_TO_FLAGGING['classify-only'], 'false');
	assert.equal(MODE_TO_FLAGGING['classify-and-sort'], 'true');
});

test('buildAccountSetArgs builds the closed per-account key write and refuses anything else', () => {
	assert.deepEqual(buildAccountSetArgs('work', 'flagging', 'false'), ['account', 'set', '--json', '--', 'work', 'flagging', 'false']);
	assert.equal(buildAccountSetArgs('work', 'policy.dry_run', 'false'), null, 'only flagging and dry_run, and never a dotted key');
	assert.equal(buildAccountSetArgs('--evil', 'flagging', 'false'), null, 'a name can never read as a flag');
	assert.equal(buildAccountSetArgs('work', 'flagging', 'yes'), null, 'the value is the engine boolean word');
	assert.equal(buildAccountSetArgs('x'.repeat(65), 'flagging', 'false'), null, 'the name keeps the engine 64-char limit');
});

test('model argv builders are a closed set: fixed flags, value flags, `--` before positionals', () => {
	assert.deepEqual(buildModelDetectArgs(), ['detect', '--json']);
	assert.deepEqual(buildModelDetectArgs('http://127.0.0.1:11434'), ['detect', '--json', '--url=http://127.0.0.1:11434']);
	assert.deepEqual(buildModelPullArgs('qwen3:8b'), ['pull', '--json', '--', 'qwen3:8b']);
	assert.deepEqual(buildModelPullArgs('qwen3:8b', 'http://10.0.0.5:11434'), ['pull', '--json', '--url=http://10.0.0.5:11434', '--', 'qwen3:8b']);
	assert.deepEqual(buildModelSaveArgs('ollama', 'http://127.0.0.1:11434', 'qwen3:8b'), ['save', '--json', '--runtime=ollama', '--url=http://127.0.0.1:11434', '--model=qwen3:8b']);
	assert.equal(buildModelSaveArgs('evil', 'http://127.0.0.1:11434', 'qwen3:8b'), null, 'only the engine runtime words');
	assert.equal(buildModelPullArgs('--help'), null, 'a model id can never read as a flag');
	assert.equal(buildModelDetectArgs('imap://x y'), null, 'no spaces in an address');
	assert.equal(buildModelDetectArgs('-x'), null, 'no leading dash');
});

test('parseModelDetect trims the engine answer to the fields the step reads', () => {
	const good = parseModelDetect({
		ok: true,
		runtimes: [{ runtime: 'ollama', url: 'http://127.0.0.1:11434', reachable: true, models: ['qwen3:8b', 'nomic-embed-text'], usable: ['qwen3:8b'], error: null }],
		shared: { runtime: 'ollama', url: 'http://127.0.0.1:11434', model: 'qwen3:14b', set_by: 'rag docs', reachable: false },
	});
	assert.equal(good.ok, true);
	assert.deepEqual(good.runtimes[0].usable, ['qwen3:8b']);
	assert.equal(good.shared?.setBy, 'rag docs');
	assert.equal(good.shared?.reachable, false, 'a shared block whose server is off is kept as unreachable');
	const refused = parseModelDetect({ ok: false, error: 'no local model server answers' });
	assert.equal(refused.ok, false);
	assert.equal(refused.error, 'no local model server answers');
	assert.deepEqual(refused.runtimes, []);
});

test('parsePullLine reads the engine normalised stages, and pullProgress turns bytes into a GB bar', () => {
	assert.deepEqual(parsePullLine('{"stage":"pulling manifest"}'), { stage: 'pulling manifest', total: 0, completed: 0, text: '' });
	const line = parsePullLine('{"stage":"downloading","total":1073741824,"completed":536870912}');
	assert.deepEqual(line?.stage, 'downloading');
	assert.deepEqual(pullProgress('qwen3:8b', line ?? ({} as never)), { model: 'qwen3:8b', done: 0.5, gb: 1, pct: 50 });
	assert.equal(parsePullLine('{"stage":"error","error":"manifest not found"}')?.text, 'manifest not found');
	assert.equal(parsePullLine('{"stage":"other","status":"verifying sha256"}')?.stage, 'other');
	assert.equal(parsePullLine('not json at all'), null, 'only the engine normalised lines reach here');
	assert.equal(pullProgress('qwen3:8b', { stage: 'success', total: 0, completed: 0, text: '' }), null, 'no bar after the download finished');
});

test('setup reuses account-edit for the add op, and the password is stdin only, never in argv', () => {
	const built = buildEditRequests('add', { name: 'work', host: 'imap.gmail.com', username: 'me@gmail.com', port: 993, tls: 'tls', password: 'sek ret' });
	assert.ok('requests' in built, 'a valid add builds requests');
	const req = built.requests[0];
	assert.ok(req.args.includes('--password-stdin'), 'the password comes from stdin');
	assert.ok(!req.args.some((a) => a.includes('sek ret')), 'the password is never part of argv');
	assert.equal(req.stdin, 'sek ret\n', 'the password is the whole stdin line');
});

test('parseConfigGlobal reads the engine defaults when a field is absent or nonsense', () => {
	assert.deepEqual(parseConfigGlobal({}), { dryRun: true, digestEnabled: true, digestTime: '07:00' });
	assert.deepEqual(parseConfigGlobal({ dry_run: false, digest: { enabled: false, time: '08:30' } }), { dryRun: false, digestEnabled: false, digestTime: '08:30' });
	assert.deepEqual(parseConfigGlobal({ digest: { time: 'nonsense' } }), { dryRun: true, digestEnabled: true, digestTime: '07:00' });
});
