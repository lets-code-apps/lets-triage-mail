#!/usr/bin/env node
// A stand-in for the real engine, used only by the smoke test (LTM_ENGINE_PATH points here).
// Synthetic: it prints fixed text and reacts to signals; it never touches mail, a model or a DB.
//
// Modes (by argv):
//   run                     long-lived; on SIGTERM exits 0 (proves the desktop's one SIGTERM landed)
//   run --exit-1            exits 1 at once (simulates a failed start -> desktop shows `failed`)
//   run --ignore-sigterm    ignores SIGTERM forever (simulates the SIGKILL-after-deadline path)
//   run --emit-logs         floods stdout with 600 lines (ring-buffer cap check)
//   check --json            prints fixtures/check-report.json
//   status --json           prints fixtures/status-report.json
//   status --exit-1         prints nothing and exits 1 (the popup shows the engine's own reason)
//   --version               prints `lets-triage-mail 0.1.0`
//
// Real engine behaviour it mirrors: first SIGTERM is a clean cancel (exit 0); the process never
// exits on its own while healthy. See src/cli/run.rs::watch_for_signals.

import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const args = process.argv.slice(2);
// An ES module has no `__dirname`; this fixture's own URL is the way to find its folder.
const HERE = dirname(fileURLToPath(import.meta.url));

const fixture = (name) => readFileSync(join(HERE, name), 'utf8');

if (args[0] === '--version') {
	process.stdout.write('lets-triage-mail 0.1.0\n');
	process.exit(0);
}

if (args[0] === 'check') {
	process.stdout.write(fixture('check-report.json'));
	process.exit(0);
}

if (args[0] === 'status') {
	if (args.includes('--exit-1')) {
		// The engine refusing to answer: no stdout, a reason on stderr, non-zero exit.
		process.stderr.write('status: cannot open the state database\n');
		process.exit(1);
	}
	process.stdout.write(fixture('status-report.json'));
	process.exit(0);
}

if (args[0] === 'run') {
	if (args.includes('--exit-1')) {
		process.stderr.write('engine: config missing\n');
		process.exit(1);
	}
	if (args.includes('--emit-logs')) {
		for (let i = 0; i < 600; i += 1) process.stdout.write(`log line ${i}\n`);
	}
	process.stderr.write('engine: watching example.com\n');

	if (args.includes('--ignore-sigterm')) {
		process.on('SIGTERM', () => {
			// Ignore it on purpose: the desktop must escalate to SIGKILL within the deadline.
		});
	} else {
		process.on('SIGTERM', () => {
			process.stdout.write('engine: received SIGTERM, cleaning up\n');
			process.exit(0);
		});
	}

	// Stay alive until signalled, so the desktop observes a running process.
	setInterval(() => {}, 60_000);
}
