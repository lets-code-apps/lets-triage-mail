import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { ConfigAccount } from '../src/account-edit.ts';
import { buildEditRequests } from '../src/account-edit.ts';
import {
	MODE_TO_DRY_RUN,
	accountNameFor,
	buildConfigSetArgs,
	decideSetup,
	guessServer,
	parseConfigGlobal,
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
	assert.deepEqual(buildConfigSetArgs('policy.dry_run', 'true'), ['config', 'set', 'policy.dry_run', 'true', '--json']);
	assert.deepEqual(buildConfigSetArgs('digest.enabled', 'false'), ['config', 'set', 'digest.enabled', 'false', '--json']);
	assert.deepEqual(buildConfigSetArgs('digest.time', '08:00'), ['config', 'set', 'digest.time', '08:00', '--json']);
	assert.equal(buildConfigSetArgs('policy.bogus', 'true'), null, 'unknown key');
	assert.equal(buildConfigSetArgs('policy.dry_run', 'maybe'), null, 'bad value');
	assert.equal(buildConfigSetArgs('digest.time', '8am'), null, 'bad time');
	assert.equal(buildConfigSetArgs('--evil', 'x'), null, 'a flag is not a key');
	assert.equal(buildConfigSetArgs('digest.time', '--nope'), null, 'a value can never start with a dash');
});

test('mode maps to the policy dry_run the engine sets', () => {
	assert.equal(MODE_TO_DRY_RUN['classify-only'], 'true');
	assert.equal(MODE_TO_DRY_RUN['classify-and-sort'], 'false');
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
