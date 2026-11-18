// The report the `desktop-shots` job writes (`desktop/shots-out/report.json`) and the exit code it
// produces. Pure and Electron-free on purpose: `node --test test/shots-report.test.ts` proves the whole
// pass/fail decision — including the failure path the plan asked for (a rendering rejection is recorded
// and turns the job red) — without a GUI. `render.ts` owns the I/O (writing the file, printing the step
// summary); this module owns the decision, exactly like `compare.ts` and `layout.ts` own the pixel diff
// and the layout rules.
export type ShotRow = { name: string; status: string; px: number; ratio: number };

export type Report = { ok: boolean; rows: ShotRow[]; violations: string[]; error: string | null };

// A shot is "good" only when it matched the baseline (OK) or was just written (--update). MISSING, SIZE,
// CHANGED and RENDER FAILED are all failures, so one bad shot turns the pass red.
const isGood = (status: string): boolean => status === 'OK' || status === 'UPDATED';

// Fold the rendered rows, the layout violations and any thrown error into one report. `error` is set when
// rendering threw (a timed-out load, a rejected capture, a broken page); recording it is what lets a
// failure still produce a report instead of escaping and skipping the write + exit entirely.
export function finalize(input: { rows: ShotRow[]; violations: string[]; error?: string }): Report {
	const error = input.error ?? null;
	const failedShot = input.rows.some((row) => !isGood(row.status));
	const ok = !failedShot && input.violations.length === 0 && error === null;
	return { ok, rows: input.rows, violations: input.violations, error };
}

// 0 is green, anything else is a red job. CI treats a non-zero exit as failure.
export function exitCodeFor(report: Report): number {
	return report.ok ? 0 : 1;
}
