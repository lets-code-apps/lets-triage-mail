// First-run onboarding logic that needs neither Electron nor a running engine: whether setup is
// needed at all, guessing an IMAP server from an address, naming an account safely, picking a model
// by the Mac's memory, reading the global `config show` fields that prefill the mode and digest
// steps, and building the engine argv for a `config set`. The account add/test argv is REUSED from
// account-edit.ts (do not duplicate it); the password travels to stdin there, never to argv.
//
// Pure, so the unit tests cover it. The engine validates every value again; this gate is the first
// check, not the only one, and it never decides what a value *means*.

// The account screen's trimmed view is reused for the "are there accounts?" decision (do not re-read
// the raw engine output here). ACCOUNT_NAME is the engine's own name shape, so the mode step's argv
// builder and account-edit's ops cannot drift apart.
import { ACCOUNT_NAME } from './account-edit.ts';
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

// The mode choice, as the engine writes it. Board decision 2026-11-27: classify only is the per-account
// classify-only mode (`flagging = false`), NOT `[policy] dry_run = true`. Both modes run the account
// live (`dry_run = false`) so the daily digest is appended to the inbox as the design draws it; only
// "classify and sort" sets flag colours on the server, which is why only its continue shows the go-live
// confirm, and declining falls back to classify only.
export type ModeChoice = 'classify-only' | 'classify-and-sort';
export const MODE_TO_FLAGGING: Record<ModeChoice, 'true' | 'false'> = {
	'classify-only': 'false',
	'classify-and-sort': 'true',
};

// The two per-account keys one mode choice writes for each configured account.
export type AccountSetChange = { key: 'flagging' | 'dry_run'; value: 'true' | 'false' };
export function modeChanges(choice: ModeChoice): AccountSetChange[] {
	return [{ key: 'flagging', value: MODE_TO_FLAGGING[choice] }, { key: 'dry_run', value: 'false' }];
}

// The engine argv for one per-account key write: `config account set --json -- NAME KEY VALUE`.
// Closed key set, boolean values, and the name shape account-edit.ts enforces (ACCOUNT_NAME), so
// nothing a page typed can read as a flag. The go-live confirm is decided by the caller per choice
// (account-edit's `set` op confirms on `dry_run=false`; the mode step's confirm belongs to "classify
// and sort" instead, so the mode op does not go through that op).
export function buildAccountSetArgs(name: unknown, key: unknown, value: unknown): string[] | null {
	if (typeof name !== 'string' || !ACCOUNT_NAME.test(name)) return null;
	if (key !== 'flagging' && key !== 'dry_run') return null;
	if (value !== 'true' && value !== 'false') return null;
	return ['account', 'set', '--json', '--', name, key, value];
}

// The runtimes `model save --runtime` accepts (the Rust `Runtime` enum in src/shared_model.rs).
export const MODEL_RUNTIMES = ['ollama', 'lmstudio', 'custom'] as const;
// The one page "install Ollama" may open. main hands it to `shell.openExternal` with no argument built
// from user input, so this stays a fixed constant and not a URL the page can change (DECISIONS.md
// 2026-11-24 rejected openExternal only for the Mail buttons, which need folder names).
export const OLLAMA_INSTALL_URL = 'https://ollama.com/download';

const MODEL_ID = /^[A-Za-z0-9:._-]{1,128}$/;
const URL_TEXT = /^[^\s\0]{1,254}$/;
const runtimeWord = (v: unknown): v is string => typeof v === 'string' && (MODEL_RUNTIMES as readonly string[]).includes(v);
const urlText = (v: unknown): v is string => typeof v === 'string' && URL_TEXT.test(v) && !v.startsWith('-');
const modelId = (v: unknown): v is string => typeof v === 'string' && MODEL_ID.test(v) && !v.startsWith('-');

// `model detect [--url ADDR] --json`: no model id; `--url` only when the page typed one.
export function buildModelDetectArgs(url?: unknown): string[] | null {
	if (url === undefined || url === null || url === '') return ['detect', '--json'];
	if (!urlText(url)) return null;
	return ['detect', '--json', `--url=${url}`];
}

// `model pull --json [--url ADDR] -- MODEL`: the model id is a positional after `--`, the value flags
// use `--flag=value`, exactly as account-edit.ts does.
export function buildModelPullArgs(model: unknown, url?: unknown): string[] | null {
	if (!modelId(model)) return null;
	if (url !== undefined && url !== null && url !== '' && !urlText(url)) return null;
	return ['pull', '--json', ...(url ? [`--url=${url}`] : []), '--', model];
}

// `model save --json --runtime R --url U --model M`: all three are validated values.
export function buildModelSaveArgs(runtime: unknown, url: unknown, model: unknown): string[] | null {
	if (!runtimeWord(runtime) || !urlText(url) || !modelId(model)) return null;
	return ['save', '--json', `--runtime=${runtime}`, `--url=${url}`, `--model=${model}`];
}

// What `model detect --json` answered (src/cli/model.rs `detect_json`, shape-locked by a Rust test):
// one probe per runtime plus the shared file's block, if any. Trimmed to the fields the step reads.
export type RuntimeProbe = { runtime: string; url: string; reachable: boolean; usable: string[]; error: string | null };
export type SharedModelBlock = { runtime: string; url: string; model: string; setBy: string; reachable: boolean };
export type ModelDetect = { ok: boolean; runtimes: RuntimeProbe[]; shared: SharedModelBlock | null; error: string | null };

export function parseModelDetect(raw: Record<string, unknown>): ModelDetect {
	if (raw.ok !== true) {
		return { ok: false, runtimes: [], shared: null, error: typeof raw.error === 'string' ? raw.error : 'the engine gave no answer' };
	}
	const probe = (v: unknown): RuntimeProbe | null => {
		if (typeof v !== 'object' || v === null || typeof (v as RuntimeProbe).url !== 'string') return null;
		const found = v as Record<string, unknown>;
		return {
			runtime: typeof found.runtime === 'string' ? found.runtime : 'custom',
			url: String(found.url),
			reachable: found.reachable === true,
			usable: Array.isArray(found.usable) ? found.usable.filter((id) => typeof id === 'string') : [],
			error: typeof found.error === 'string' ? found.error : null,
		};
	};
	const block = raw.shared && typeof raw.shared === 'object' && !Array.isArray(raw.shared) ? (raw.shared as Record<string, unknown>) : null;
	return {
		ok: true,
		runtimes: (Array.isArray(raw.runtimes) ? raw.runtimes : []).flatMap((v) => {
			const p = probe(v);
			return p === null ? [] : [p];
		}),
		shared:
			block === null
				? null
				: {
						runtime: typeof block.runtime === 'string' ? block.runtime : 'custom',
						url: typeof block.url === 'string' ? block.url : '',
						model: typeof block.model === 'string' ? block.model : '',
						setBy: typeof block.set_by === 'string' ? block.set_by : '',
						reachable: block.reachable === true,
					},
		error: null,
	};
}

// One normalised `model pull` line (`{"stage": ...}`, src/model_setup.rs normalise_pull_line). The
// desktop never re-reads raw Ollama text: the engine already shaped it.
export type PullLine = { stage: 'pulling manifest' | 'downloading' | 'success' | 'error' | 'other'; total: number; completed: number; text: string };

export function parsePullLine(line: string): PullLine | null {
	let parsed: unknown;
	try {
		parsed = JSON.parse(line);
	} catch {
		return null;
	}
	if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) return null;
	const obj = parsed as Record<string, unknown>;
	const known = ['pulling manifest', 'downloading', 'success', 'error', 'other'] as const;
	const stage = known.includes(obj.stage as (typeof known)[number]) ? (obj.stage as PullLine['stage']) : 'other';
	return {
		stage,
		total: typeof obj.total === 'number' ? obj.total : 0,
		completed: typeof obj.completed === 'number' ? obj.completed : 0,
		text: typeof obj.error === 'string' ? obj.error : typeof obj.status === 'string' ? obj.status : '',
	};
}

// The progress bar for a `downloading` line: bytes become GB (the design's bar is labelled in GB),
// and pct comes from the engine's own numbers, never a guess. (The module-level GB is the same
// constant recommendedModel sizes against.)
const round1 = (x: number): number => Math.round(x * 10) / 10;
export function pullProgress(model: string, line: PullLine): { model: string; done: number; gb: number; pct: number } | null {
	if (line.stage !== 'downloading' || line.total <= 0) return null;
	return { model, done: round1(line.completed / GB), gb: round1(line.total / GB), pct: Math.round((line.completed / line.total) * 100) };
}

