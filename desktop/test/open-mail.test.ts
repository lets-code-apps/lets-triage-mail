import assert from 'node:assert/strict';
import test from 'node:test';

import { openMail } from '../src/open-mail.ts';

// The whole contract of the popup's count buttons in one test: they open Apple Mail itself through a
// fixed argv, and a failure never throws into the IPC handler. The injected `run` records the call,
// so this needs no real `open` and never launches an application.

test('openMail runs open -a Mail with a fixed argv, exactly once', async () => {
	const calls: Array<[string, string[]]> = [];
	const fake = (file: string, args: readonly string[], cb: (error?: Error | null) => void) => {
		calls.push([file, [...args]]);
		cb(undefined); // success
	};

	await openMail(fake);

	assert.equal(calls.length, 1, 'the page cannot cause more than one launch');
	assert.deepEqual(calls[0], ['open', ['-a', 'Mail']]);
});

test('a failing open resolves quietly instead of rejecting', async () => {
	// Mail missing (never on macOS, but the IPC handler must not see a rejection or a throw).
	const fake = (file: string, args: readonly string[], cb: (error?: Error | null) => void) => {
		void file;
		void args;
		cb(new Error('open: no such application'));
	};

	await openMail(fake); // must not throw

	// And a synchronous throw from the runner is swallowed too, so openMail still resolves.
	await openMail((file, args) => {
		void file;
		void args;
		throw new Error('spawn exploded');
	});
});
