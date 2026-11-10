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

test('the headline is the "to look at" total, with a pill that follows it and the engine', () => {
	const view = buildPopupView(report(), OPTS);
	assert.equal(view.headline, '4');
	assert.deepEqual([view.pill, view.pillTone], ['needs reply', 'attention']);
	const clear = buildPopupView(oneAccountWith(0, 'connected'), OPTS);
	assert.deepEqual([clear.pill, clear.pillTone], ['all clear', 'positive']);
});

test('the subline carries the other two buckets and the window', () => {
	const view = buildPopupView(report(), OPTS);
	// 1793998800 is 21:00 in UTC; the popup says "since", not "today until now".
	assert.equal(view.subline, '6 fyi · 12 done · since 21:00');
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
	assert.deepEqual([view.pill, view.pillTone], ['not running', 'muted']);
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
	assert.equal(view.headline, '0');
});

test('the clock line follows the pinned zone, not the machine', () => {
	const view = buildPopupView(report(), { locale: 'en-US', timeZone: 'Europe/Berlin' });
	// 1793998800 is 22:00 in Berlin, 21:00 in UTC: the zone is an argument, never the machine's.
	assert.match(view.subline, /since \d\d:\d\d$/);
	assert.notEqual(view.subline, buildPopupView(report(), OPTS).subline);
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
