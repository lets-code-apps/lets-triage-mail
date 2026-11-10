// What the accounts screen may ask the engine to do, checked at the trust boundary (main process).
//
// The page sends `(op, fields)`; this module turns validated fields into the engine's argv and
// stdin. Renderer strings never become flags: option values use the `--flag=value` form and the
// positional ones come after `--`. The password goes to stdin only. Pure (no electron, no
// spawning), so the unit tests cover it. The engine validates again; this is the first gate, not
// the only one, and it never decides what a valid setting *means*.

import { parseCheckReport, type CheckReport } from './check-report.ts';

export const ACCOUNT_OPS = ['add', 'set', 'remove', 'password', 'test'] as const;
export type AccountOp = (typeof ACCOUNT_OPS)[number];

// Mirrors SETTABLE_KEYS in src/config_edit.rs.
export const SETTABLE_KEYS = ['host', 'port', 'username', 'tls', 'inbox', 'flagging', 'watch_junk', 'dry_run'] as const;
const TLS_WORDS = ['tls', 'starttls', 'none'];

export type EditRequest = {
	args: string[];
	stdin?: string;
	// Ask the user first (native dialog in main): going live, or removing an account.
	confirm?: 'live' | 'remove';
	// The call signs in to a real server and needs the long timeout.
	slow?: boolean;
};

export type EditResult = {
	ok: boolean;
	error?: string;
	// What happened to the engine after a change: restarted by the app, running outside the app
	// (the page tells the user to restart it), or nothing to do.
	restart?: 'done' | 'external' | 'none';
	report?: CheckReport;
};

type Fields = Record<string, unknown>;
const isRecord = (v: unknown): v is Fields => typeof v === 'object' && v !== null && !Array.isArray(v);

// The engine's allowed name characters (check_account_name in src/config_edit.rs), at most 64, and
// starting with a letter or digit so a name can never read as a flag (belt and braces with `--`).
const NAME = /^[A-Za-z0-9][A-Za-z0-9 _.@-]{0,63}$/;
const HOST = /^[^\s\0]{1,253}$/;
const PRINTABLE = /^[^\0\r\n]*$/;

const name = (f: Fields): string | null => (typeof f.name === 'string' && NAME.test(f.name) ? f.name : null);
const host = (f: Fields): string | null =>
	typeof f.host === 'string' && HOST.test(f.host) && !f.host.startsWith('-') ? f.host : null;
const username = (f: Fields): string | null =>
	typeof f.username === 'string' && f.username.length >= 1 && f.username.length <= 254 && PRINTABLE.test(f.username) ? f.username : null;
const password = (f: Fields): string | null =>
	typeof f.password === 'string' && f.password.length >= 1 && f.password.length <= 1024 && PRINTABLE.test(f.password) ? f.password : null;
// Optional fields: absent is fine, present must be valid.
const port = (f: Fields): number | null | undefined =>
	f.port === undefined || f.port === '' ? undefined : typeof f.port === 'number' && Number.isInteger(f.port) && f.port >= 1 && f.port <= 65535 ? f.port : null;
const tls = (f: Fields): string | null | undefined =>
	f.tls === undefined ? undefined : typeof f.tls === 'string' && TLS_WORDS.includes(f.tls) ? f.tls : null;

const bad = (message: string): { error: string } => ({ error: message });

// `--host=... --username=... [--port=...] [--tls=...] --password-stdin` for add and the ad-hoc test.
function connection(f: Fields): { args: string[]; stdin: string } | { error: string } {
	const h = host(f);
	const u = username(f);
	const p = port(f);
	const t = tls(f);
	const pw = password(f);
	if (h === null) return bad('The server name is missing or not valid.');
	if (u === null) return bad('The user name is missing or not valid.');
	if (p === null) return bad('The port must be a number from 1 to 65535.');
	if (t === null) return bad('The connection security must be tls, starttls or none.');
	if (pw === null) return bad('The password is missing.');
	return {
		args: [`--host=${h}`, `--username=${u}`, ...(p === undefined ? [] : [`--port=${p}`]), ...(t === undefined ? [] : [`--tls=${t}`]), '--password-stdin'],
		stdin: `${pw}\n`,
	};
}

// A list because `set` takes several `{key, value}` changes (a mode change is two keys) and the
// engine is restarted once afterwards. Every other op is one request. `args` follow `config`.
export function buildEditRequests(op: unknown, fields: unknown): { requests: EditRequest[] } | { error: string } {
	if (!ACCOUNT_OPS.includes(op as AccountOp) || !isRecord(fields)) return bad('Not a known account action.');
	const n = name(fields);
	const one = (request: EditRequest) => ({ requests: [request] });
	switch (op as AccountOp) {
		case 'add': {
			const c = connection(fields);
			if (n === null) return bad('The account name needs letters, digits, spaces or - _ . @ (up to 64).');
			if ('error' in c) return c;
			return one({ args: ['account', 'add', '--json', `--name=${n}`, ...c.args], stdin: c.stdin });
		}
		case 'test': {
			// An existing account (just its name) or typed-in values (nothing is stored).
			if (fields.host === undefined) {
				if (n === null) return bad('The account name is not valid.');
				return one({ args: ['account', 'test', '--json', '--', n], slow: true });
			}
			const c = connection(fields);
			if ('error' in c) return c;
			return one({ args: ['account', 'test', '--json', ...c.args], stdin: c.stdin, slow: true });
		}
		case 'remove':
			if (n === null) return bad('The account name is not valid.');
			return one({ args: ['account', 'remove', '--json', '--forget-password', '--', n], confirm: 'remove' });
		case 'password': {
			const pw = password(fields);
			if (n === null) return bad('The account name is not valid.');
			if (pw === null) return bad('The password is missing.');
			return one({ args: ['account', 'password', '--json', '--password-stdin', '--', n], stdin: `${pw}\n` });
		}
		case 'set': {
			if (n === null) return bad('The account name is not valid.');
			const changes = fields.changes;
			if (!Array.isArray(changes) || changes.length < 1 || changes.length > 4) return bad('Nothing to change.');
			const requests: EditRequest[] = [];
			for (const change of changes) {
				if (!isRecord(change) || !SETTABLE_KEYS.includes(change.key as (typeof SETTABLE_KEYS)[number])) return bad('That setting cannot be changed here.');
				const v = change.value;
				const text = typeof v === 'boolean' || typeof v === 'number' ? String(v) : v;
				if (typeof text !== 'string' || text.length > 254 || !PRINTABLE.test(text)) return bad('That value is not valid.');
				const live = change.key === 'dry_run' && text === 'false';
				requests.push({ args: ['account', 'set', '--json', '--', n, String(change.key), text], ...(live ? { confirm: 'live' as const } : {}) });
			}
			return { requests };
		}
	}
}

// What the accounts screen needs from `config show --json`, and nothing else: the engine's
// answer is typed and trimmed here. `{ok:false, error}` passes through as a message.
export type ConfigAccount = { name: string; host: string; port: number; username: string; tls: string; mode: string; passwordSource: string };
export type ConfigView = { ok: true; accounts: ConfigAccount[] } | { ok: false; error: string; missing: boolean };

export function parseConfigShow(raw: Record<string, unknown>): ConfigView {
	if (raw.ok !== true) {
		return { ok: false, error: typeof raw.error === 'string' ? raw.error : 'the engine gave no answer', missing: raw.missing === true };
	}
	const accounts: ConfigAccount[] = [];
	for (const a of Array.isArray(raw.accounts) ? raw.accounts : []) {
		if (!isRecord(a)) continue;
		const { name: nm, host: h, port: p, username: u, tls: t, mode, password_source: src } = a;
		if (typeof nm !== 'string' || typeof h !== 'string' || typeof p !== 'number' || typeof u !== 'string') continue;
		accounts.push({
			name: nm,
			host: h,
			port: p,
			username: u,
			tls: typeof t === 'string' ? t : 'tls',
			mode: typeof mode === 'string' ? mode : '',
			passwordSource: typeof src === 'string' ? src : '',
		});
	}
	return { ok: true, accounts };
}

// An engine reply to a write or a test, as the page gets it. A check report is re-parsed so only
// typed items reach the renderer.
export function toEditResult(op: AccountOp, raw: Record<string, unknown>): EditResult {
	if (op === 'test') {
		const parsed = parseCheckReport(JSON.stringify(raw));
		if ('report' in parsed) return { ok: parsed.report.ok, report: parsed.report };
	}
	if (raw.ok === true) return { ok: true, ...(raw.restart_needed === true ? { restart: 'none' as const } : {}) };
	return { ok: false, error: typeof raw.error === 'string' ? raw.error : 'the engine gave no answer' };
}
