// First-run onboarding logic that needs neither Electron nor a running engine: whether setup is
// needed at all, guessing an IMAP server from an address, naming an account safely, picking a model
// by the Mac's memory, reading the global `config show` fields that prefill the mode and digest
// steps, and building the engine argv for a `config set`. The account add/test argv is REUSED from
// account-edit.ts (do not duplicate it); the password travels to stdin there, never to argv.
//
// Pure, so the unit tests cover it. The engine validates every value again; this gate is the first
// check, not the only one, and it never decides what a value *means*.

// The account screen's trimmed view is reused for the "are there accounts?" decision (do not re-read
// the raw engine output here).
import type { ConfigView } from './account-edit.ts';

// A `config show --json` carries the policy dry-run and the [digest] enabled/time that the accounts
// screen ignores. Steps 2 (mode) and 4 (digest) prefill from them; the defaults match the engine's,
// which are safe (dry-run on, digest on, 07:00).
export type GlobalView = { dryRun: boolean; digestEnabled: boolean; digestTime: string };

export function parseConfigGlobal(raw: Record<string, unknown>): GlobalView {
	const digest = raw.digest && typeof raw.digest === 'object' ? (raw.digest as Record<string, unknown>) : {};
	return {
		dryRun: raw.dry_run !== false,
		digestEnabled: digest.enabled !== false,
		digestTime: typeof digest.time === 'string' && /^\d{2}:\d{2}$/.test(digest.time) ? digest.time : '07:00',
	};
}

// The four onboarding steps, in fixed order. The page owns the step number.
export const SETUP_STEPS = ['accounts', 'mode', 'model', 'digest'] as const;

// Should the window open the setup flow on launch?
// - setupDone already recorded -> skip (a second launch shows the normal screen).
// - the config read failed and the file is *not* missing (it is broken) -> skip; the normal screen
//   shows the error, and setup must not paper over a broken file.
// - no config file at all (missing) or an empty config (no accounts) -> setup.
// - accounts already exist but setup was never recorded -> silent: record setupDone, open nothing.
export function decideSetup(setupDone: boolean, config: ConfigView): 'setup' | 'silent' | 'skip' {
	if (setupDone) return 'skip';
	if (!config.ok) return config.missing ? 'setup' : 'skip';
	return config.accounts.length === 0 ? 'setup' : 'silent';
}

// Guess the IMAP server (and its standard TLS port) from an email address. The four big providers
// have known hosts; anything else is the plain `imap.<domain>` convention. Both stay editable.
const SERVER_BY_DOMAIN: Record<string, string> = {
	'gmail.com': 'imap.gmail.com',
	'googlemail.com': 'imap.gmail.com',
	'icloud.com': 'imap.mail.me.com',
	'me.com': 'imap.mail.me.com',
	'mac.com': 'imap.mail.me.com',
	'fastmail.com': 'imap.fastmail.com',
	'fastmail.fm': 'imap.fastmail.com',
	'outlook.com': 'outlook.office365.com',
	'hotmail.com': 'outlook.office365.com',
	'live.com': 'outlook.office365.com',
	'msn.com': 'outlook.office365.com',
};

export function guessServer(address: string): { host: string; port: number } {
	const domain = (address.split('@').pop() ?? address).trim().toLowerCase();
	const host = Object.prototype.hasOwnProperty.call(SERVER_BY_DOMAIN, domain) ? SERVER_BY_DOMAIN[domain] : `imap.${domain}`;
	return { host, port: 993 };
}

// There is no name field in the UI, so the account name is derived from the address. Chars the
// engine's check_account_name refuses become '-'; the name starts with a letter or digit, is at most
// 64 chars, and on a clash gains " 2", " 3", ... without going over 64.
function sanitizeBase(text: string): string {
	const swapped = text.replace(/[^A-Za-z0-9 _.@-]/g, '-');
	const trimmed = swapped.replace(/^[^A-Za-z0-9]+/, '');
	return trimmed.length > 0 ? trimmed : 'account';
}

export function accountNameFor(address: string, existing: string[]): string {
	const local = address.split('@')[0] || address.split('@').pop() || address;
	const base = sanitizeBase(local).slice(0, 64);
	let candidate = base;
	for (let n = 2; existing.includes(candidate); n += 1) {
		const suffix = ` ${n}`;
		candidate = `${base.slice(0, Math.max(0, 64 - suffix.length))}${suffix}`;
	}
	return candidate;
}

// Pick the recommended model by the Mac's memory. The engine's own thresholds win over any mockup:
// a small Mac gets the small model, so a pull never tries to load more than the machine has.
const GB = 1024 ** 3;
export function recommendedModel(totalBytes: number): { model: string; sizeGb: number } {
	const mem = totalBytes / GB;
	if (mem < 12) return { model: 'qwen3:4b', sizeGb: 2.5 };
	if (mem < 32) return { model: 'qwen3:8b', sizeGb: 5.2 };
	return { model: 'qwen3:14b', sizeGb: 9.3 };
}

// The keys onboarding may write with `config set`. Mirrors SETTABLE_GLOBAL_KEYS in src/config_edit.rs.
export const GLOBAL_SET_KEYS = ['policy.dry_run', 'digest.enabled', 'digest.time'] as const;

// Build the engine argv for one `config set KEY VALUE --json`. A closed key list, a value that can
// never read as a flag (never starts with '-', printable, shape-checked), and a `--flag`-free argv.
export function buildConfigSetArgs(key: unknown, value: unknown): string[] | null {
	if (typeof key !== 'string' || !(GLOBAL_SET_KEYS as readonly string[]).includes(key)) return null;
	if (typeof value !== 'string' || value.startsWith('-') || !/^[A-Za-z0-9:.]+$/.test(value)) return null;
	const ok =
		key === 'policy.dry_run' || key === 'digest.enabled'
			? value === 'true' || value === 'false'
			: /^\d{2}:\d{2}$/.test(value);
	if (!ok) return null;
	return ['config', 'set', key, value, '--json'];
}

// The mode choice, as the engine argument it becomes. classify-only keeps everything safe; classify
// and sort is the go-live choice that shows a confirm on the continue path.
export type ModeChoice = 'classify-only' | 'classify-and-sort';
export const MODE_TO_DRY_RUN: Record<ModeChoice, 'true' | 'false'> = {
	'classify-only': 'true',
	'classify-and-sort': 'false',
};
