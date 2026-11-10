// Turns a parsed `status --json` report into strings that are ready to print.
//
// Why this exists (docs/DESKTOP.md): the popup's renderer is a dumb, sandboxed page. Every
// sentence, number format and colour choice is decided here, in a pure module that plain
// `node --test` can exercise without Electron. The renderer only assigns `textContent`.
//
// Hostile-input rule: `name` and `reason` came from the engine, which got them from a config
// file. They are copied verbatim (never re-worded) and the popup must write them with
// `textContent`, so no HTML can be smuggled in. `state` decides only a colour class, chosen from
// a fixed list in `popup.css` — an unknown state is muted, not trusted.

import { KNOWN_STATES } from './status-report.ts';
import type { AccountState, StatusAccount, StatusReport } from './status-report.ts';

export type Tone = 'positive' | 'attention' | 'danger' | 'muted';

// One line of the popup: what the user reads per account.
export type AccountRow = {
	name: string;
	mode: string;
	note: string;
	state: string;
	tone: Tone;
	// Two capital letters for the window's avatar circle, and the account's own counts line.
	initials: string;
	counts: string;
};

export type PopupView = {
	// The big number ("4") and the pill beside it ("needs reply", "all clear", "not running").
	headline: string;
	pill: string;
	pillTone: Tone;
	// "2 fyi · 7 done · since 21:00"
	subline: string;
	// "1 of 3 connected", or the engine-down / no-accounts copy.
	summary: string;
	accounts: AccountRow[];
	tone: Tone;
	// Whether the popup's Start button is disabled (true when an engine runs outside the app).
	startDisabled: boolean;
};

const isKnown = (state: string): boolean => (KNOWN_STATES as readonly string[]).includes(state);

// Which colour one account pill gets. Fixed here so the popup can never be told a colour by data.
const toneFor = (state: AccountState): Tone => {
	switch (state) {
		case 'connected':
			return 'positive';
		// Working, but not settled yet: worth a look, not alarming.
		case 'connecting':
		case 'reconnecting':
		case 'sign_in_failed':
			return 'attention';
		case 'failed':
			return 'danger';
		// "Engine not running" is a fact, not a problem; unknown states read as muted too.
		case 'not_running':
			return 'muted';
	}
};

const toneForState = (state: string): Tone => (isKnown(state) ? toneFor(state as AccountState) : 'muted');

// HH:MM of `since`, in the machine's zone unless the caller pins one (tests pin UTC).
const clock = (epochSeconds: number, locale: string, timeZone: string): string =>
	new Intl.DateTimeFormat(locale, { hour: '2-digit', minute: '2-digit', timeZone, hour12: false }).format(
		new Date(epochSeconds * 1000),
	);

// The plan's sketch also passed a wall clock. It is dropped deliberately: the report already
// carries `generated_at`, and a second clock to agree with is a second thing to test.
// `ownsEngine` defaults to true (the app spawned the engine it is describing); main passes false
// when a fresh status shows an engine running that the app did not start — then Start is refused.
export function buildPopupView(
	report: StatusReport,
	options: { locale: string; timeZone: string; ownsEngine?: boolean },
): PopupView {
	const { locale, timeZone } = options;
	const ownsEngine = options.ownsEngine ?? true;
	const since = clock(report.window.since, locale, timeZone);
	const counts = report.counts;
	const subline = `${counts.fyi} fyi · ${counts.done} done · since ${since}`;

	const running = report.engine.running;
	const [pill, pillTone]: [string, Tone] = !running
		? ['not running', 'muted']
		: counts.to_look_at > 0
			? ['needs reply', 'attention']
			: ['all clear', 'positive'];
	// An engine is running, but not one this app spawned (launchd or a terminal): Start must be
	// refused, and the popup says where the live engine came from instead of "X of Y connected".
	const external = running && !ownsEngine;

	if (report.accounts.length === 0) {
		// The config has no accounts: nothing can be connected, and the numbers are all zero.
		return {
			headline: '0',
			pill,
			pillTone,
			subline: `0 fyi · 0 done · since ${since}`,
			summary: external
				? 'Running outside the app. No accounts configured.'
				: 'No accounts configured.',
			accounts: [],
			tone: 'muted',
			startDisabled: external,
		};
	}

	const connected = report.accounts.filter((account) => account.state === 'connected').length;
	const total = report.accounts.length;
	// Engine down: every stored state is stale, so the engine says `not_running`; the popup says
	// so in its own words, and the counts (which do come from the database) stay trustworthy.
	const summary = external
		? 'Running outside the app.'
		: running
			? `${connected} of ${total} connected`
			: 'Engine not running. Counts are from the database.';

	const accounts = report.accounts.map((account) => accountRow(account));

	return {
		headline: String(counts.to_look_at),
		pill,
		pillTone,
		subline,
		summary,
		accounts,
		tone: overallTone(accounts, running),
		startDisabled: external,
	};
}

const accountRow = (account: StatusAccount): AccountRow => ({
	name: account.name,
	mode: account.mode,
	// The engine's own fixed English sentence for this state (never raw server error text).
	note: account.reason,
	// Human label for the pill; the tone below still reads the raw state.
	state: account.state.replaceAll('_', ' '),
	tone: toneForState(account.state),
	initials: initials(account.name),
	counts: `${account.counts.to_look_at} to look at · ${account.counts.fyi} fyi · ${account.counts.done} done`,
});

// First letters of the first two words, uppercased ("Example Work" -> "EW"). Spread, not
// indexing, so an emoji or other astral character is not cut in half.
export const initials = (name: string): string =>
	name
		.split(/\s+/)
		.filter(Boolean)
		.slice(0, 2)
		.map((word) => [...word][0]!.toUpperCase())
		.join('');

// The popup's accent colour: the worst thing any account is saying.
const overallTone = (accounts: AccountRow[], running: boolean): Tone => {
	if (!running) return 'muted';
	if (accounts.some((row) => row.tone === 'danger')) return 'danger';
	if (accounts.some((row) => row.tone === 'attention')) return 'attention';
	return accounts.every((row) => row.tone === 'positive') ? 'positive' : 'muted';
};

