// Opens Apple Mail itself when one of the popup's count buttons is pressed. The plan is explicit:
// no folder, no per-message target, nothing the page can influence — just the fixed program and its
// two literal arguments. That is the TypeScript form of the same safety the Rust engine relies on:
// run a fixed argv with `execFile` (no shell), never `exec` (one string through a shell), so no
// part of the command can be interpreted by a shell and nothing from the renderer is ever interpolated.
//
// `run` is injected so `test/open-mail.test.ts` can watch the call and force a failure without ever
// opening a real application; the default is Node's own `execFile`.

import { execFile } from 'node:child_process';

// The slice of `execFile` this module needs: a program, literal arguments, and an error-first callback.
type OpenRunner = (file: string, args: readonly string[], callback: (error?: Error | null) => void) => void;

export function openMail(run: OpenRunner = (file, args, callback) => execFile(file, args, callback)): Promise<void> {
	return new Promise<void>((resolve) => {
		try {
			// The argv is a literal, built here; the caller passes nothing. A missing `open` (never on
			// macOS, where this app runs) or Mail refusing is an error on the callback, not a throw.
			run('open', ['-a', 'Mail'], (error) => {
				if (error) console.error(`openMail: could not open Apple Mail (${error.message}).`);
				resolve();
			});
		} catch {
			// A synchronous throw from the runner is still not allowed to reach the IPC handler.
			console.error('openMail: could not open Apple Mail.');
			resolve();
		}
	});
}
