// The two first-run notification switches, as pure logic. main polls the engine's own `status` every
// 30 seconds while a switch is on and feeds each report here; this module decides *whether* to tell
// the user about a change and *how many* times, nothing more. It never invents mail text, a sender or a
// subject, and it does no I/O: `notify` (an Electron Notification built only in main) and `now` are
// injected, so the rules are unit-tested without Electron.
//
// - Needs a reply: one notification per message that newly needs a reply, per account, capped at five
//   per poll, then one "and N more". The first read of a window is a baseline (no notifications); a
//   changed window or a lower count re-baselines silently instead of notifying.
// - Connection problems: when an account stays offline (reconnecting / sign-in failed / failed) for
//   five continuous minutes, one notification per outage, re-armed when it connects again.
// A silent-skip install turns both switches off, so nothing is ever sent.

import type { StatusReport } from './status-report.ts';

export type Notify = (title: string, body: string) => void;

// States that count as "offline" for the connection-problems switch. `connecting` is transient and
// `not_running` is not the user's problem, so neither starts or extends an outage.
const OFFLINE = ['reconnecting', 'sign_in_failed', 'failed'];
const OFFLINE_MS = 5 * 60 * 1000; // an outage is worth mentioning only after five continuous minutes
const CAP = 5; // at most five individual needs-a-reply notifications per poll

export function createNotifier(opts: {
	notify: Notify;
	now: () => number;
	notifyReply: boolean;
	notifyOffline: boolean;
}): { poll(report: StatusReport): void } {
	// Per-account baseline of "how many needed a reply last time we looked".
	const lastLook = new Map<string, number>();
	// The window these baselines belong to; a change means the counts are not comparable.
	let windowSince: number | null = null;
	// Offline tracking per account: when the outage started, and whether we already reported it.
	const offlineSince = new Map<string, number>();
	const offlineReported = new Set<string>();
	// Has any report been seen at all? The very first one is only a baseline.
	let primed = false;

	const poll = (report: StatusReport): void => {
		// A new window (or the first report): re-baseline silently, send nothing this pass.
		if (windowSince !== report.window.since) {
			windowSince = report.window.since;
			lastLook.clear();
			primed = false;
		}

		// Needs a reply: compare each account's "needs a reply" count to the baseline.
		if (opts.notifyReply) {
			const rising: string[] = [];
			for (const account of report.accounts) {
				const before = lastLook.get(account.name);
				const nowCount = account.counts.to_look_at;
				if (before === undefined) {
					lastLook.set(account.name, nowCount); // first time this account is seen: baseline
					continue;
				}
				if (nowCount < before) {
					lastLook.set(account.name, nowCount); // a lower count resets the baseline, no noise
					continue;
				}
				for (let i = 0; i < nowCount - before; i += 1) rising.push(account.name);
				lastLook.set(account.name, nowCount);
			}
			if (primed) {
				const shown = rising.slice(0, CAP);
				for (const name of shown) opts.notify('needs a reply', `${name}: a new message needs a reply`);
				const remaining = rising.length - shown.length;
				if (remaining > 0) opts.notify('needs a reply', `and ${remaining} more`);
			}
		}

		// Connection problems: an outage is reported once, only after five continuous minutes.
		if (opts.notifyOffline) {
			const nowMs = opts.now();
			for (const account of report.accounts) {
				if (OFFLINE.includes(account.state)) {
					if (!offlineSince.has(account.name)) offlineSince.set(account.name, nowMs);
					const started = offlineSince.get(account.name) ?? nowMs;
					if (!offlineReported.has(account.name) && nowMs - started >= OFFLINE_MS) {
						opts.notify('still offline', `${account.name}: triage mail could not reach your mail`);
						offlineReported.add(account.name);
					}
				} else if (account.state === 'connected') {
					// Re-armed: a fresh outage starts its own timer and may be reported again.
					offlineSince.delete(account.name);
					offlineReported.delete(account.name);
				}
			}
		}

		primed = true;
	};

	return { poll };
}
