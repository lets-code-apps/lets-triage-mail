// Parses the output of `lets-triage-mail status --json`.
//
// The engine's `src/status.rs` locks this shape with its own test (`json_shape_is_locked`);
// renaming a key or value there is a breaking change for the desktop, which is why the shape
// version is checked rather than ignored. Engine output is *data, not code*: a malformed or hostile
// line must become a typed value or a clean error string, never a crash. We only trust the types;
// the engine owns the meaning of every state and reason (see check-report.ts for the same rule).

export const STATUS_VERSION = 1;

export type Counts = { to_look_at: number; fyi: number; done: number };

// The six states `status.rs` writes. The parser keeps any other string (an engine newer than this
// app, or a hand-edited row) rather than guessing: `popup-model.ts` renders an unknown state muted.
export const KNOWN_STATES = [
	'connecting',
	'connected',
	'reconnecting',
	'sign_in_failed',
	'failed',
	'not_running',
] as const;

export type AccountState = (typeof KNOWN_STATES)[number];

export type StatusAccount = {
	name: string;
	mode: string;
	state: string;
	reason: string;
	counts: Counts;
};

export type StatusEngine = {
	running: boolean;
	started_at: number | null;
	heartbeat_at: number | null;
};

export type StatusReport = {
	version: number;
	generated_at: number;
	engine: StatusEngine;
	window: { kind: string; since: number };
	counts: Counts;
	accounts: StatusAccount[];
};

export type ParseResult = { report: StatusReport } | { error: string };

const isRecord = (value: unknown): value is Record<string, unknown> =>
	typeof value === 'object' && value !== null && !Array.isArray(value);

// A finite number where the report promises one. `null` is only allowed where the engine sends
// `null` (the two engine timestamps when it is not running).
const num = (value: unknown): value is number => typeof value === 'number' && Number.isFinite(value);

// The three counters appear in every `counts` object, both the totals and per account.
const counts = (value: unknown): value is Counts =>
	isRecord(value) && num(value.to_look_at) && num(value.fyi) && num(value.done);

export function parseStatusReport(stdout: string): ParseResult {
	const text = stdout.trim();
	if (text === '') {
		return { error: 'status --json produced no output' };
	}

	let parsed: unknown;
	try {
		parsed = JSON.parse(text);
	} catch {
		return { error: 'status --json did not print valid JSON' };
	}

	if (!isRecord(parsed)) {
		return { error: 'status report is not an object' };
	}

	// The engine refuses to guess a shape it did not produce, either: a different version means
	// the keys below may mean something else.
	if (parsed.version !== STATUS_VERSION) {
		return { error: `status report has unsupported version ${String(parsed.version)}` };
	}
	if (!num(parsed.generated_at)) {
		return { error: 'status report is missing a numeric "generated_at"' };
	}

	if (!isRecord(parsed.engine)) {
		return { error: 'status report is missing an "engine" object' };
	}
	if (typeof parsed.engine.running !== 'boolean') {
		return { error: 'engine is missing a boolean "running"' };
	}
	// `null` only when the engine is not running; a number otherwise. Anything else is rejected.
	const time = (value: unknown): boolean => value === null || num(value);
	if (!time(parsed.engine.started_at) || !time(parsed.engine.heartbeat_at)) {
		return { error: 'engine timestamps are neither numbers nor null' };
	}

	if (!isRecord(parsed.window) || !num(parsed.window.since)) {
		return { error: 'status report is missing a "window" with a numeric "since"' };
	}
	if (!counts(parsed.counts)) {
		return { error: 'status report totals are not three numbers' };
	}
	if (!Array.isArray(parsed.accounts)) {
		return { error: 'status report has no "accounts" array' };
	}

	const accounts: StatusAccount[] = [];
	for (const [index, raw] of parsed.accounts.entries()) {
		if (!isRecord(raw)) {
			return { error: `account ${index} is not an object` };
		}
		if (typeof raw.name !== 'string' || typeof raw.mode !== 'string') {
			return { error: `account ${index} is missing "name" or "mode"` };
		}
		// Hostile text stays hostile data: name and reason are kept verbatim for the renderer, which
		// must write them with textContent.
		if (typeof raw.state !== 'string' || typeof raw.reason !== 'string') {
			return { error: `account ${index} is missing a "state" or "reason" string` };
		}
		if (!counts(raw.counts)) {
			return { error: `account ${index} has counts that are not three numbers` };
		}
		accounts.push({
			name: raw.name,
			mode: raw.mode,
			state: raw.state,
			reason: raw.reason,
			counts: {
				to_look_at: raw.counts.to_look_at,
				fyi: raw.counts.fyi,
				done: raw.counts.done,
			},
		});
	}

	return {
		report: {
			version: parsed.version,
			generated_at: parsed.generated_at,
			engine: {
				running: parsed.engine.running,
				started_at: parsed.engine.started_at as number | null,
				heartbeat_at: parsed.engine.heartbeat_at as number | null,
			},
			window: { kind: String(parsed.window.kind), since: parsed.window.since },
			counts: {
				to_look_at: parsed.counts.to_look_at,
				fyi: parsed.counts.fyi,
				done: parsed.counts.done,
			},
			accounts,
		},
	};
}

// True when a state string is one this app knows how to colour. Unknown strings still display.
export function isKnownState(state: string): boolean {
	return (KNOWN_STATES as readonly string[]).includes(state);
}
