import assert from 'node:assert/strict';
import { test } from 'node:test';

import { createNotifier } from '../src/notifier.ts';

const MIN = 60 * 1000;

// A `status --json` report, only as far as the notifier reads it: a window stamp, and per-account
// state + the "needs a reply" count. Everything else is present but ignored by the notifier.
const report = (
	since: number,
	accounts: { name: string; state: string; toLookAt: number }[],
) => ({
	version: 1,
	generated_at: 0,
	engine: { running: true, started_at: null, heartbeat_at: null },
	window: { kind: 'day', since },
	counts: { to_look_at: 0, fyi: 0, done: 0 },
	accounts: accounts.map((a) => ({
		name: a.name,
		mode: '',
		state: a.state,
		reason: '',
		counts: { to_look_at: a.toLookAt, fyi: 0, done: 0 },
	})),
});

// Drive a notifier: a mutable clock, and every notification collected in order.
class Harness {
	msgs: { title: string; body: string }[] = [];
	nowMs = 0;
	private notifier = createNotifier({
		notify: (title, body) => this.msgs.push({ title, body }),
		now: () => this.nowMs,
		notifyReply: true,
		notifyOffline: true,
	});
	poll(since: number, accounts: { name: string; state: string; toLookAt: number }[]) {
		this.notifier.poll(report(since, accounts) as never);
	}
}

test('the first read is a baseline and sends nothing', () => {
	const h = new Harness();
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 2 }]);
	assert.deepEqual(h.msgs, []);
});

test('a rise in the needs-a-reply count sends one per message; a lower count resets silently', () => {
	const h = new Harness();
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 2 }]); // baseline
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 4 }]); // two more need a reply
	assert.equal(h.msgs.length, 2);
	for (const m of h.msgs) assert.deepEqual(m, { title: 'needs a reply', body: 'work: a new message needs a reply' });
	h.msgs.length = 0;
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 1 }]); // fewer: reset, no noise
	assert.deepEqual(h.msgs, []);
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 3 }]); // two more than the new baseline
	assert.equal(h.msgs.length, 2);
});

test('at most five needs-a-reply per poll, then one summary', () => {
	const h = new Harness();
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 0 }]); // baseline
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 7 }]); // seven new
	assert.equal(h.msgs.length, 6);
	assert.equal(h.msgs.filter((m) => m.body === 'work: a new message needs a reply').length, 5);
	assert.equal(h.msgs[5].body, 'and 2 more');
});

test('a changed window re-baselines silently (day rollover sends nothing)', () => {
	const h = new Harness();
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 0 }]); // baseline, window 1
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 3 }]); // three new -> 3 notifications
	assert.equal(h.msgs.length, 3);
	h.poll(2, [{ name: 'work', state: 'connected', toLookAt: 10 }]); // new window: reset, send nothing
	assert.equal(h.msgs.length, 3, 'the window-change poll must not notify');
	h.poll(2, [{ name: 'work', state: 'connected', toLookAt: 12 }]); // two new in the new window
	assert.equal(h.msgs.length, 5);
});

test('a five-minute offline is reported once, then re-armed on connect', () => {
	const h = new Harness();
	h.nowMs = 0;
	h.poll(1, [{ name: 'work', state: 'reconnecting', toLookAt: 0 }]); // outage starts at 0, nothing yet
	assert.equal(h.msgs.filter((m) => m.title === 'still offline').length, 0);
	h.nowMs = 5 * MIN;
	h.poll(1, [{ name: 'work', state: 'reconnecting', toLookAt: 0 }]); // five minutes in -> one
	assert.equal(h.msgs.filter((m) => m.title === 'still offline').length, 1);
	h.nowMs = 6 * MIN;
	h.poll(1, [{ name: 'work', state: 'sign_in_failed', toLookAt: 0 }]); // still the same outage -> no second
	assert.equal(h.msgs.filter((m) => m.title === 'still offline').length, 1);
	h.nowMs = 7 * MIN;
	h.poll(1, [{ name: 'work', state: 'connected', toLookAt: 0 }]); // re-armed
	h.nowMs = 7 * MIN;
	h.poll(1, [{ name: 'work', state: 'reconnecting', toLookAt: 0 }]); // new outage starts at 7m
	assert.equal(h.msgs.filter((m) => m.title === 'still offline').length, 1);
	h.nowMs = 12 * MIN;
	h.poll(1, [{ name: 'work', state: 'reconnecting', toLookAt: 0 }]); // five minutes into the new one -> again
	assert.equal(h.msgs.filter((m) => m.title === 'still offline').length, 2);
});

test('switches off send nothing at all', () => {
	let nowMs = 0;
	const off = createNotifier({
		notify: () => assert.fail('no notification expected when the switches are off'),
		now: () => nowMs,
		notifyReply: false,
		notifyOffline: false,
	});
	// A baseline, then a big needs-a-reply rise AND a five-minute outage, both with the switches off.
	nowMs = 0;
	off.poll(report(1, [{ name: 'work', state: 'connected', toLookAt: 0 }]) as never);
	nowMs = 10 * MIN;
	off.poll(report(1, [{ name: 'work', state: 'failed', toLookAt: 6 }]) as never);
	assert.ok(true); // reaching here means nothing was sent
});
