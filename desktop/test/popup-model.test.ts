import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { buildPopupView, initials } from '../src/popup-model.ts';
import type { Tone } from '../src/popup-model.ts';
import { parseStatusReport } from '../src/status-report.ts';
import type { StatusReport } from '../src/status-report.ts';

const FIXTURE = join(import.meta.dirname, 'fixtures', 'status-report.json');

const report = (): StatusReport => {
	const parsed = parseStatusReport(readFileSync(FIXTURE, 'utf8'));
	if ('error' in parsed) throw new Error(parsed.error);
	return parsed.report;
};

// Options used by every test below: a pinned zone makes the clock line deterministic.
const OPTS = { locale: 'en-GB', timeZone: 'UTC' };


test('the fixture report renders one row per account, verbatim', () => {
	const view = buildPopupView(report(), OPTS);
	assert.equal(view.accounts.length, 2);
	assert.equal(view.accounts[0].name, 'Example Work');
	assert.equal(view.accounts[0].mode, 'live');
	assert.equal(view.accounts[0].note, 'Watching the inbox.');
	assert.equal(view.accounts[1].state, 'sign in failed');
});

test('the strip is three cells (needs reply, fyi, done) with the totals for the day', () => {
	const view = buildPopupView(report(), OPTS);
	assert.deepEqual(
		view.strip.map((c) => [c.key, c.label, c.count]),
		[
			['to_look_at', 'needs reply', '4'],
			['fyi', 'fyi', '6'],
			['done', 'done', '12'],
		],
	);
	// The dot is the strip's only colour, and only on needs reply with a count.
	assert.equal(view.strip[0].tone, 'attention');
	assert.equal(view.strip[1].tone, null);
	assert.equal(view.strip[2].tone, null);
	// In the calm popup (nothing needs a reply) no cell has a tone.
	const calm = buildPopupView(oneAccountWith(0, 'connected'), OPTS);
	assert.deepEqual(calm.strip.map((c) => c.tone), [null, null, null]);
});

test('the note is "N done since HH:MM" when something needs a reply', () => {
	const view = buildPopupView(report(), OPTS);
	// 1793998800 is 21:00 in UTC; the note counts what is done since the day started.
	assert.equal(view.note, '12 done since 21:00.');
});

test('the calm note says nothing needs a reply', () => {
	const view = buildPopupView(oneAccountWith(0, 'connected'), OPTS);
	assert.ok(view.note.startsWith('Nothing needs a reply. 0 done since'));
});

test('"connected" counts accounts, in the "1 of 3" form', () => {
	const view = buildPopupView(report(), OPTS);
	assert.equal(view.summary, '1 of 2 connected');
});

test('each state picks its own pill colour', () => {
	const tones: [string, Tone][] = [
		['connected', 'positive'],
		['connecting', 'attention'],
		['reconnecting', 'attention'],
		['sign_in_failed', 'attention'],
		['failed', 'danger'],
		['not_running', 'muted'],
	];
	for (const [state, tone] of tones) {
		const view = buildPopupView(oneAccount(state), OPTS);
		assert.equal(view.accounts[0].tone, tone, state);
	}
});

test('an unknown state is muted, never coloured', () => {
	const view = buildPopupView(oneAccount('warming_up'), OPTS);
	assert.equal(view.accounts[0].tone, 'muted');
});

test('the popup accent is the worst thing any account says', () => {
	assert.equal(buildPopupView(oneAccount('failed'), OPTS).tone, 'danger');
	assert.equal(buildPopupView(oneAccount('sign_in_failed'), OPTS).tone, 'attention');
	assert.equal(buildPopupView(oneAccount('connected'), OPTS).tone, 'positive');
});

test('engine down: accounts read not_running and the copy says so', () => {
	const raw = JSON.parse(readFileSync(FIXTURE, 'utf8')) as Record<string, unknown>;
	const engine = raw.engine as Record<string, unknown>;
	engine.running = false;
	engine.started_at = null;
	engine.heartbeat_at = null;
	const parsed = parseStatusReport(JSON.stringify(raw));
	if ('error' in parsed) throw new Error(parsed.error);
	const view = buildPopupView(parsed.report, OPTS);
	assert.equal(view.summary, 'Engine not running. Counts are from the database.');
	// The accent is muted: nothing is alarming, nothing is live.
	assert.equal(view.tone, 'muted');
	// Not paused (the app did not press Stop): the strip still shows the counts and the note reads
	// as if it were running, only the summary names the engine being down.
	assert.equal(view.paused, false);
	assert.deepEqual(view.strip.map((c) => c.count), ['4', '6', '12']);
	assert.equal(view.note, '12 done since 21:00.');
});

test('no accounts configured: a calm empty popup', () => {
	const raw = JSON.parse(readFileSync(FIXTURE, 'utf8')) as Record<string, unknown>;
	raw.accounts = [];
	raw.counts = { to_look_at: 0, fyi: 0, done: 0 };
	const parsed = parseStatusReport(JSON.stringify(raw));
	if ('error' in parsed) throw new Error(parsed.error);
	const view = buildPopupView(parsed.report, OPTS);
	assert.equal(view.summary, 'No accounts configured.');
	assert.deepEqual(view.accounts, []);
	assert.deepEqual(view.strip.map((c) => c.count), ['0', '0', '0']);
	assert.equal(view.note, 'Nothing needs a reply. 0 done since 21:00.');
	assert.equal(view.paused, false);
});

test('the note time follows the pinned zone, not the machine', () => {
	const berlin = buildPopupView(report(), { locale: 'en-US', timeZone: 'Europe/Berlin' });
	// 1793998800 is 22:00 in Berlin, 21:00 in UTC: the zone is an argument, never the machine's.
	assert.equal(berlin.note, '12 done since 22:00.');
	assert.notEqual(berlin.note, buildPopupView(report(), OPTS).note);
});

test('an engine started outside the app disables Start and says so', () => {
	// `status` says an engine is running, but it is not this app's child (launchd or a terminal):
	// Start is refused and the copy names where the live engine came from, not "X of Y connected".
	const external = buildPopupView(report(), { ...OPTS, ownsEngine: false });
	assert.equal(external.startDisabled, true);
	assert.equal(external.summary, 'Running outside the app.');
	// The app's own engine (the default) keeps Start enabled.
	assert.equal(buildPopupView(report(), OPTS).startDisabled, false);
	// Nothing running at all: Start stays enabled (no watcher to double up).
	const down = buildPopupView(report(), { ...OPTS, ownsEngine: false, });
	assert.notEqual(down.startDisabled, undefined);
});

// A report with exactly one account in the given state (totals match it, so the accent is honest).
function oneAccount(state: string) {
	return oneAccountWith(2, state);
}

function oneAccountWith(toLookAt: number, state: string) {
	const base = report();
	return {
		...base,
		counts: { to_look_at: toLookAt, fyi: 0, done: 0 },
		accounts: [
			{ ...base.accounts[0], state, counts: { to_look_at: toLookAt, fyi: 0, done: 0 } },
		],
	};
}

test('initials: first letters of the first two words, uppercased', () => {
	assert.equal(initials('Example Work'), 'EW');
	assert.equal(initials('personal'), 'P');
	assert.equal(initials('  my  home office '), 'MH');
	assert.equal(initials(''), '');
});

test('each row carries its own counts line', () => {
	const row = buildPopupView(oneAccountWith(4, 'connected'), OPTS).accounts[0];
	assert.equal(row.counts, '4 to look at · 0 fyi · 0 done');
	assert.equal(row.initials, 'EW');
});

// The accessible name of a cell repeats its count, so a screen reader hears the number, not just the
// bucket label. The zero case is what the strip shows when a bucket is empty.
test('every cell names itself with its count, for a screen reader', () => {
	const view = buildPopupView(report(), OPTS);
	assert.deepEqual(
		view.strip.map((c) => c.aria),
		[
			'needs reply, 4, open Apple Mail',
			'fyi, 6, open Apple Mail',
			'done, 12, open Apple Mail',
		],
	);
	const zero = buildPopupView(oneAccountWith(0, 'connected'), OPTS);
	assert.equal(zero.strip[0].aria, 'needs reply, 0, open Apple Mail');
});

// A helper: the fixture with the engine reported not-running (a stop, not a start), reused by the
// paused and crash cases below.
function stoppedReport(): StatusReport {
	const raw = JSON.parse(readFileSync(FIXTURE, 'utf8')) as Record<string, unknown>;
	const engine = raw.engine as Record<string, unknown>;
	engine.running = false;
	engine.started_at = null;
	engine.heartbeat_at = null;
	const parsed = parseStatusReport(JSON.stringify(raw));
	if ('error' in parsed) throw new Error(parsed.error);
	return parsed.report;
}

test('paused with a time: paused true and the "paused at" note', () => {
	// 0 is 00:00 in UTC; the note quotes the pinned instant, then the fixed sentence. The strip is
	// untouched by pausing: same counts, the dot still follows needs reply > 0.
	const view = buildPopupView(stoppedReport(), { ...OPTS, paused: { at: 0 } });
	assert.equal(view.paused, true);
	assert.equal(view.note, 'Paused at 00:00. New mail stays in the inbox until you resume.');
	assert.deepEqual(view.strip.map((c) => c.count), ['4', '6', '12']);
	assert.equal(view.strip[0].tone, 'attention');
});

test('paused with an unknown time (the app restarted) drops the time', () => {
	const view = buildPopupView(stoppedReport(), { ...OPTS, paused: { at: null } });
	assert.equal(view.paused, true);
	assert.equal(view.note, 'Paused. New mail stays in the inbox until you resume.');
});

test('the paused option is ignored while the engine is running', () => {
	// buildPopupView's contract: a running engine is never "paused", whatever the option says.
	const view = buildPopupView(report(), { ...OPTS, paused: { at: 0 } });
	assert.equal(view.paused, false);
	assert.equal(view.note, '12 done since 21:00.');
});
