// The one setting the app stores itself (the rest live in macOS or the engine's config): a tiny
// pure module so the parse and the allow-list can be tested without Electron. File IO is in main.

export const APPEARANCES = ['system', 'dark', 'light'] as const;
export type Appearance = (typeof APPEARANCES)[number];
export type Settings = { appearance: Appearance };

export const DEFAULT_SETTINGS: Settings = { appearance: 'system' };

export const isAppearance = (value: unknown): value is Appearance => APPEARANCES.includes(value as Appearance);

// A missing, corrupt or unknown file is the defaults, never an error.
export function parseSettings(text: string): Settings {
	try {
		const parsed = JSON.parse(text) as { appearance?: unknown } | null;
		if (isAppearance(parsed?.appearance)) return { appearance: parsed.appearance };
	} catch {
		// fall through to the defaults
	}
	return DEFAULT_SETTINGS;
}

// What `setSetting` may do: a known name with a value of the right type, nothing else.
export const isAllowedSetting = (name: unknown, value: unknown): boolean =>
	(name === 'appearance' && isAppearance(value)) || (name === 'openAtLogin' && typeof value === 'boolean');
