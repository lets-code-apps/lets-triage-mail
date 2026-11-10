import assert from 'node:assert/strict';
import { test } from 'node:test';

import { buildEditRequests, parseConfigShow, toEditResult } from '../src/account-edit.ts';

const ADD = { name: 'Work mail', host: 'imap.example.com', port: 993, username: 'me@example.com', tls: 'tls', password: 'hunter2-pw' };

const one = (op: string, fields: unknown) => {
	const built = buildEditRequests(op, fields);
	assert.ok('requests' in built, JSON.stringify(built));
	return built.requests[0];
};

test('add builds the engine argv from validated fields and keeps the password on stdin', () => {
	const request = one('add', ADD);
	assert.deepEqual(request.args, ['account', 'add', '--json', '--name=Work mail', '--host=imap.example.com', '--username=me@example.com', '--port=993', '--tls=tls', '--password-stdin']);
	assert.equal(request.stdin, 'hunter2-pw\n');
});

test('a password is never part of any argv, for every op that takes one', () => {
	for (const [op, fields] of [['add', ADD], ['password', { name: 'work', password: ADD.password }], ['test', ADD]] as const) {
		const request = one(op, fields);
		assert.ok(!request.args.join(' ').includes(ADD.password), `${op} leaked the password into argv`);
		assert.equal(request.stdin, `${ADD.password}\n`);
	}
});

test('a name that looks like a flag is refused, and positionals come after --', () => {
	for (const op of ['remove', 'password', 'test', 'add']) {
		// (a test with typed-in values ignores the name, so it gets the bare-name form)
		assert.ok('error' in buildEditRequests(op, op === 'test' ? { name: '--config' } : { ...ADD, name: '--config' }), op);
	}
	assert.deepEqual(one('remove', { name: 'work' }).args, ['account', 'remove', '--json', '--forget-password', '--', 'work']);
	assert.deepEqual(one('test', { name: 'work' }).args, ['account', 'test', '--json', '--', 'work']);
});

test('fields are checked: host, port, tls, username, password, name length', () => {
	const bad = (patch: Record<string, unknown>) => assert.ok('error' in buildEditRequests('add', { ...ADD, ...patch }), JSON.stringify(patch));
	bad({ host: 'imap example.com' });
	bad({ host: '--help' });
	bad({ host: 'a'.repeat(254) });
	bad({ port: 0 });
	bad({ port: 70000 });
	bad({ port: 1.5 });
	bad({ port: '993' });
	bad({ tls: 'ssl' });
	bad({ username: '' });
	bad({ username: 'u'.repeat(255) });
	bad({ password: '' });
	bad({ password: 'two\nlines' });
	bad({ password: 5 });
	bad({ name: 'x'.repeat(65) });
	bad({ name: "it's" });
	assert.ok('error' in buildEditRequests('nuke', ADD));
	assert.ok('error' in buildEditRequests('add', null));
});

test('port and tls are optional on add', () => {
	const { port, tls, ...rest } = ADD;
	void port;
	void tls;
	assert.deepEqual(one('add', rest).args.slice(-3), ['--host=imap.example.com', '--username=me@example.com', '--password-stdin']);
});

test('test with typed-in values is slow and stores nothing; with a name it needs no password', () => {
	const typed = one('test', ADD);
	assert.equal(typed.slow, true);
	assert.ok(typed.args.includes('--password-stdin') && !typed.args.includes('--name=Work mail'));
	assert.equal(one('test', { name: 'work' }).stdin, undefined);
});

test('set only takes the engine settable keys, as separate requests, and going live needs a confirm', () => {
	const built = buildEditRequests('set', { name: 'work', changes: [{ key: 'flagging', value: true }, { key: 'dry_run', value: false }] });
	assert.ok('requests' in built);
	assert.deepEqual(built.requests.map((r) => r.args), [
		['account', 'set', '--json', '--', 'work', 'flagging', 'true'],
		['account', 'set', '--json', '--', 'work', 'dry_run', 'false'],
	]);
	assert.deepEqual(built.requests.map((r) => r.confirm), [undefined, 'live']);
	assert.ok('error' in buildEditRequests('set', { name: 'work', changes: [{ key: 'password_command', value: 'x' }] }));
	assert.ok('error' in buildEditRequests('set', { name: 'work', changes: [] }));
	assert.equal(one('set', { name: 'work', changes: [{ key: 'dry_run', value: true }] }).confirm, undefined);
});

test('remove asks for a confirmation and forgets the password', () => {
	assert.equal(one('remove', { name: 'work' }).confirm, 'remove');
});

test('config show is trimmed to what the screen needs; failures become a message', () => {
	const view = parseConfigShow({
		ok: true,
		path: '/x',
		accounts: [
			{ name: 'work', host: 'imap.example.com', port: 993, username: 'me', tls: 'tls', mode: 'dry-run', password_source: 'keychain', inbox: 'INBOX' },
			{ name: 5 },
		],
	});
	assert.deepEqual(view, { ok: true, accounts: [{ name: 'work', host: 'imap.example.com', port: 993, username: 'me', tls: 'tls', mode: 'dry-run', passwordSource: 'keychain' }] });
	assert.deepEqual(parseConfigShow({ ok: false, error: 'no config file at /x', missing: true }), { ok: false, error: 'no config file at /x', missing: true });
});

test('edit results: a test returns the typed report, a write says whether a restart is needed', () => {
	const report = { ok: false, failed: 1, warnings: 0, items: [{ area: 'test', status: 'fail', message: 'cannot connect', hint: null }] };
	assert.deepEqual(toEditResult('test', report), { ok: false, report });
	assert.deepEqual(toEditResult('add', { ok: true, restart_needed: true }), { ok: true, restart: 'none' });
	assert.deepEqual(toEditResult('password', { ok: true, restart_needed: false }), { ok: true });
	assert.deepEqual(toEditResult('add', { ok: false, error: 'duplicate' }), { ok: false, error: 'duplicate' });
});
