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
//   status --json           prints fixtures/status-report.json (needs reply 4)
//   status --calm           prints fixtures/status-report-calm.json (needs reply 0 everywhere)
//   status --paused         prints today's counts with engine.running false (a stopped engine)
//   status --exit-1         prints nothing and exits 1 (the popup shows the engine's own reason)
//   (--calm / --paused also honoured by env LTM_FAKE_STATUS=calm|paused, so the smoke test can pick
//    a state without changing the argv it passes.)
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
	// Which popup state to report: a flag wins, else the smoke test may set LTM_FAKE_STATUS (the desktop
	// always runs `status --json`, so an env var is the only way it can ask for a different state).
	const mode = args.includes('--calm')
		? 'calm'
		: args.includes('--paused')
			? 'paused'
			: process.env.LTM_FAKE_STATUS === 'calm' || process.env.LTM_FAKE_STATUS === 'paused'
				? process.env.LTM_FAKE_STATUS
				: 'normal';

	if (mode === 'calm') {
		process.stdout.write(fixture('status-report-calm.json'));
		process.exit(0);
	}
	if (mode === 'paused') {
		// Same day's counts, but the engine is stopped: the popup's paused copy depends on this and on
		// the desktop remembering the Stop (in-memory pausedAt).
		const report = JSON.parse(fixture('status-report.json'));
		report.engine.running = false;
		report.engine.started_at = null;
		report.engine.heartbeat_at = null;
		process.stdout.write(JSON.stringify(report));
		process.exit(0);
	}
	process.stdout.write(fixture('status-report.json'));
	process.exit(0);
}

// --- the first-run onboarding commands -------------------------------------------------------------
// Setup asks the engine things the accounts screen never does: read the config, read the model situation,
// then write both. The JSON shapes mirror the real engine exactly (src/cli/config.rs::show_json,
// src/cli/model.rs::detect_json and save_json, model_setup.rs::normalise_pull_line), with synthetic values
// only: imap.example.com, /example/config.toml, qwen3:*. Nothing is stored anywhere; a write is echoed.
//
// Scenarios come from the ENVIRONMENT, because the desktop always passes the same argv:
//   LTM_FAKE_CONFIG=missing|empty|account   (default account)
//   LTM_FAKE_MODEL=shared|found|found-empty|not-found|pull-fail   (default found)
// The password an add or password op carries arrives on stdin; it is read and thrown away, exactly as a
// real engine would consume it, so nothing the desktop writes to the pipe is left dangling.

const FAKE_ACCOUNT = {
	name: 'Example Work',
	host: 'imap.example.com',
	port: 993,
	username: 'work@example.com',
	tls: 'tls',
	inbox: 'INBOX',
	flagging: true,
	watch_junk: false,
	dry_run: false,
	mode: 'live',
	password_source: 'keyring',
};

// `config show --json` in its three onboarding states. `missing` is the true first run (no file yet),
// `empty` is a file with no accounts, `account` is the normal screen's config.
const configShow = () => {
	const scenario = process.env.LTM_FAKE_CONFIG ?? 'account';
	if (scenario === 'missing') {
		process.stdout.write(JSON.stringify({ ok: false, error: 'no config file at /example/config.toml', missing: true }));
		process.exit(1);
	}
	process.stdout.write(
		JSON.stringify({
			ok: true,
			path: '/example/config.toml',
			dry_run: true,
			llm: { base_url: 'http://localhost:11434', model: 'qwen3:8b' },
			digest: { enabled: true, time: '07:00' },
			accounts: scenario === 'account' ? [FAKE_ACCOUNT] : [],
		}),
	);
	process.exit(0);
};

if (args[0] === 'config') {
	if (args[1] === 'show') configShow();
	// A stdin-reading op: the desktop pipes the password in, so wait for it to close before answering.
	const takeStdin = () => new Promise((resolve) => {
		process.stdin.resume();
		process.stdin.on('data', () => {});
		process.stdin.on('end', resolve);
		process.stdin.on('close', resolve);
	});
	if (args[1] === 'account' || args[1] === 'set') {
		void takeStdin().then(() => {
			process.stdout.write(JSON.stringify({ ok: true, restart_needed: true }));
			process.exit(0);
		});
	}
}

// `model detect --json`: what answers, per scenario. One entry per runtime, plus the shared file's block.
const RUNTIME_PROBE = (reachable, usable, error) => ({
	runtime: 'ollama',
	url: 'http://localhost:11434',
	reachable,
	models: usable,
	usable,
	error,
});

if (args[0] === 'model') {
	const scenario = process.env.LTM_FAKE_MODEL ?? 'found';
	if (args[1] === 'detect') {
		const shared = scenario === 'shared' ? { runtime: 'ollama', url: 'http://localhost:11434', model: 'qwen3:8b', set_by: 'rag docs', reachable: true } : null;
		const runtimes =
			scenario === 'not-found'
				? [RUNTIME_PROBE(false, [], 'no server answered at http://localhost:11434')]
				: scenario === 'found-empty'
					? [RUNTIME_PROBE(true, [], null)]
					: [RUNTIME_PROBE(true, ['qwen3:8b', 'llama3.2:3b'], null)];
		process.stdout.write(JSON.stringify({ ok: true, runtimes, shared }));
		process.exit(0);
	}
	if (args[1] === 'save') {
		process.stdout.write(JSON.stringify({ ok: true, runtime: 'ollama', url: 'http://localhost:11434', model: 'qwen3:8b' }));
		process.exit(0);
	}
	if (args[1] === 'pull') {
		// NDJSON, one normalised line per stage (model_setup.rs::normalise_pull_line): the desktop's bar
		// reads total/completed as bytes and turns them into GB. 5.2 GB is the design's size for qwen3:8b.
		const total = 5_576_501_248;
		const step = (obj) => process.stdout.write(`${JSON.stringify(obj)}\n`);
		step({ stage: 'pulling manifest' });
		for (const pct of [18, 42, 71, 100]) step({ stage: 'downloading', total, completed: Math.round((total * pct) / 100) });
		if (scenario === 'pull-fail') {
			step({ stage: 'error', error: 'manifest not found for that model' });
			process.exit(1);
		}
		step({ stage: 'success' });
		// A real pull streams for minutes, so cancelling it is a SIGTERM the desktop sends and the engine
		// answers with exit 0; the fixture streams in one go, so cancel is checked in engine.test.ts, where
		// the child is a fixture that stays alive until signalled.
		process.exit(0);
	}
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
