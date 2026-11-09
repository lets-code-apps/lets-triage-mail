// Parses the output of `lets-triage-mail check --json`.
//
// The engine's `check.rs` locks this shape with its own test (~line 1404); renaming a field
// there is a breaking change for the desktop. We re-check the shape here defensively because
// engine output is *data, not code*: a malformed or hostile line must become a typed value or a
// clean error string, never a crash, and never something we re-word or re-validate. The engine
// owns the meaning of `ok`/`failed`/`warnings`; we only trust the types.

export type CheckStatus = 'ok' | 'warn' | 'fail';

export type CheckItem = {
	area: string;
	status: CheckStatus;
	message: string;
	hint: string | null;
};

export type CheckReport = {
	ok: boolean;
	failed: number;
	warnings: number;
	items: CheckItem[];
};

export type ParseResult = { report: CheckReport } | { error: string };

const isRecord = (value: unknown): value is Record<string, unknown> =>
	typeof value === 'object' && value !== null && !Array.isArray(value);

export function parseCheckReport(stdout: string): ParseResult {
	const text = stdout.trim();
	if (text === '') {
		return { error: 'check --json produced no output' };
	}

	let parsed: unknown;
	try {
		parsed = JSON.parse(text);
	} catch {
		return { error: 'check --json did not print valid JSON' };
	}

	if (!isRecord(parsed)) {
		return { error: 'check --json output is not an object' };
	}

	// The report header: three scalars we only type-check, never reinterpret.
	if (typeof parsed.ok !== 'boolean') {
		return { error: 'check report is missing a boolean "ok"' };
	}
	if (typeof parsed.failed !== 'number' || !Number.isFinite(parsed.failed)) {
		return { error: 'check report is missing a numeric "failed"' };
	}
	if (typeof parsed.warnings !== 'number' || !Number.isFinite(parsed.warnings)) {
		return { error: 'check report is missing a numeric "warnings"' };
	}
	if (!Array.isArray(parsed.items)) {
		return { error: 'check report has no "items" array' };
	}

	const items: CheckItem[] = [];
	for (const [index, raw] of parsed.items.entries()) {
		if (!isRecord(raw)) {
			return { error: `check item ${index} is not an object` };
		}
		if (typeof raw.area !== 'string' || typeof raw.message !== 'string') {
			return { error: `check item ${index} is missing "area" or "message"` };
		}
		if (raw.status !== 'ok' && raw.status !== 'warn' && raw.status !== 'fail') {
			return { error: `check item ${index} has an unknown status` };
		}
		// A missing hint is the same as no hint; anything other than a string or null is rejected.
		const hint = raw.hint === undefined ? null : raw.hint;
		if (hint !== null && typeof hint !== 'string') {
			return { error: `check item ${index} has a non-string "hint"` };
		}
		// Pass the engine's own text straight through: no re-wording, no re-validation.
		items.push({ area: raw.area, status: raw.status, message: raw.message, hint: hint });
	}

	return {
		report: {
			ok: parsed.ok,
			failed: parsed.failed,
			warnings: parsed.warnings,
			items,
		},
	};
}
