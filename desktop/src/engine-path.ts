// Finds the Rust engine binary the desktop will spawn. Pure (no spawning), so it is trivial to
// test with a fake environment.
//
// Search order (first match wins):
//   1. `LTM_ENGINE_PATH` — a developer pointing at a specific (often debug) binary.
//   2. `<resources>/engine/lets-triage-mail` — the bundled engine in a packaged build. The
//      lookup is implemented now; nothing ships there yet (packaging is increment 9).
//   3. `<repo>/target/debug/lets-triage-mail` — the debug build the developer just compiled.
//
// We pass no `--config` unless `LTM_CONFIG` is set: the engine finds the same config and state
// files the CLI uses (its default config and database paths, per docs/SPEC.md section 3). One setup,
// two front ends.

import { existsSync } from 'node:fs';
import { join } from 'node:path';

const BINARY = 'lets-triage-mail';

export type EnginePath = { file: string; baseArgs: string[] };

export function resolveEngine(
	env: NodeJS.ProcessEnv,
	resourcesPath: string | undefined,
	repoRoot: string,
): EnginePath {
	// Global flags shared by every engine call. `--config` is global in the CLI, so it precedes
	// the subcommand; the desktop only adds it when the user overrode the config location.
	const baseArgs: string[] = [];
	const config = env.LTM_CONFIG;
	if (config) {
		baseArgs.push('--config', config);
	}

	const override = env.LTM_ENGINE_PATH;
	if (override) {
		// An explicit path is trusted as given; a missing file surfaces from the spawn, not here.
		return { file: override, baseArgs };
	}

	if (resourcesPath) {
		const packaged = join(resourcesPath, 'engine', BINARY);
		if (existsSync(packaged)) {
			return { file: packaged, baseArgs };
		}
	}

	const dev = join(repoRoot, 'target', 'debug', BINARY);
	if (existsSync(dev)) {
		return { file: dev, baseArgs };
	}

	throw new Error(
		`no lets-triage-mail engine found.\n` +
			`  tried LTM_ENGINE_PATH (unset), ${join('resources', 'engine', BINARY)} and ${dev}.\n` +
			`  build it first with \`cargo build\` from the repository root, ` +
			`or set LTM_ENGINE_PATH to a built binary.`,
	);
}
