// What the app stores itself (the rest live in macOS or the engine's config): a tiny pure module
// so the parse and the allow-list can be tested without Electron. File IO is in main.
//
// `setupDone` records that first-run onboarding finished; it is written by main (never by the
// page). `notifyReply`/`notifyOffline` are the two first-run notification switches; both default
// true (an ordinary install turns notifications on) and a "set up later"/skip install sets them
// false. They are pure preferences: the app reads the model's health through the engine's own
// status, it never invents mail text.

export const APPEARANCES = ['system', 'dark', 'light'] as const;
export type Appearance = (typeof APPEARANCES)[number];
export type Settings = { appearance: Appearance; setupDone: boolean; notifyReply: boolean; notifyOffline: boolean };

export const DEFAULT_SETTINGS: Settings = { appearance: 'system', setupDone: false, notifyReply: true, notifyOffline: true };

export const isAppearance = (value: unknown): value is Appearance => APPEARANCES.includes(value as Appearance);

// A missing, corrupt or unknown file is the defaults, never an error. A valid file keeps every
// known key it carries; unknown keys are dropped.
export function parseSettings(text: string): Settings {
	const out: Settings = { ...DEFAULT_SETTINGS };
	try {
		const parsed = JSON.parse(text) as Record<string, unknown> | null;
		if (!parsed) return DEFAULT_SETTINGS;
		if (isAppearance(parsed.appearance)) out.appearance = parsed.appearance;
		if (typeof parsed.setupDone === 'boolean') out.setupDone = parsed.setupDone;
		if (typeof parsed.notifyReply === 'boolean') out.notifyReply = parsed.notifyReply;
		if (typeof parsed.notifyOffline === 'boolean') out.notifyOffline = parsed.notifyOffline;
	} catch {
		return DEFAULT_SETTINGS;
	}
	return out;
}

// What `setSetting` may do: a known name with a value of the right type, nothing else. `setupDone`
// is deliberately absent: the page cannot mark onboarding done, only main can.
// An install that already has accounts skips onboarding. Its setup is marked done and both first-run
// switches go off (the board's decision: a silent skip is not an opt-in to notifications). Pure so the
// skip default and its survival across a relaunch (parseSettings keeps known keys) can be tested without
// Electron.
export function skippedInstallSettings(settings: Settings): Settings {
	return { ...settings, setupDone: true, notifyReply: false, notifyOffline: false };
}

export const isAllowedSetting = (name: unknown, value: unknown): boolean =>
	name === 'appearance'
		? isAppearance(value)
		: name === 'openAtLogin' || name === 'notifyReply' || name === 'notifyOffline'
			? typeof value === 'boolean'
			: false;
