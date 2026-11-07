import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { test } from 'node:test';

import { buildPopupView } from '../src/popup-model.ts';
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
	assert.equal(view.accounts[1].state, 'sign_in_failed');
});

test('the headline counts today\'s "to look at" total, pluralised', () => {
	const view = buildPopupView(report(), OPTS);
	assert.equal(view.headline, '4');
	// The number is the headline; the label only carries the noun, so it pluralises.
	assert.equal(view.headlineLabel, 'messages to look at');
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

test('one message reads "message to look at"', () => {
	const view = buildPopupView(oneAccountWith(1, 'connected'), OPTS);
	assert.equal(view.headline, '1');
	assert.equal(view.headlineLabel, 'message to look at');
});

test('the clock line follows the pinned zone, not the machine', () => {
	const view = buildPopupView(report(), { locale: 'en-US', timeZone: 'Europe/Berlin' });
	// 1793998800 is 22:00 in Berlin, 21:00 in UTC: the zone is an argument, never the machine's.
	assert.match(view.subline, /since \d\d:\d\d$/);
	assert.notEqual(view.subline, buildPopupView(report(), OPTS).subline);
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
