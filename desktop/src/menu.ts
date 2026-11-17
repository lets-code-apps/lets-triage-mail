// The tray menu as plain data. Keeping this pure (no `electron` import) means it can be unit-tested
// directly; `main.ts` turns these descriptors into a real Electron `Menu` and wires each `action`
// to a controller call. Enabled/disabled per engine state is the product's "the app offers, the
// user chooses" decision (docs/DECISIONS.md 2026-10-28).

import type { EngineState } from './ipc.ts';

export type MenuAction = 'start' | 'stop' | 'check' | 'openWindow' | 'quit' | null;

export type MenuDescriptor = {
	label: string;
	enabled: boolean;
	action: MenuAction;
	accelerator?: string | undefined;
};

export interface TrayMenuInput {
	state: EngineState;
	version: string;
	exitCode: number | null;
	signal: string | null;
}

// A human-readable engine state. `running` means the process is alive, not that mail is healthy;
// the label says "Running (check `check setup` for health)" nowhere here on purpose — that nuance
// lives in the window and docs, the tray stays short. A failure names the exit code or signal.
export function statusText(input: TrayMenuInput): string {
	switch (input.state) {
		case 'stopped':
			return 'Stopped';
		case 'starting':
			return 'Starting…';
		case 'running':
			return 'Running';
		case 'stopping':
			return 'Stopping…';
		case 'failed': {
			if (input.exitCode !== null) return `Failed (exit ${input.exitCode})`;
			if (input.signal !== null) return `Failed (${input.signal})`;
			return 'Failed';
		}
	}
}

export function buildTrayMenu(input: TrayMenuInput): MenuDescriptor[] {
	const canStart = input.state === 'stopped' || input.state === 'failed';
	const canStop = input.state === 'running';

	return [
		// App name, disabled: it is the header the tray item groups under.
		{ label: "Let's Triage Mail", enabled: false, action: null },
		{ label: statusText(input), enabled: false, action: null },
		{ label: 'Start triage', enabled: canStart, action: 'start' },
		{ label: 'Stop triage', enabled: canStop, action: 'stop' },
		{ label: 'Check setup', enabled: true, action: 'check' },
		{ label: 'Open window…', enabled: true, action: 'openWindow' },
		// Engine version, disabled: read-only info. The version string is resolved from `--version`.
		{ label: `Engine ${input.version}`, enabled: false, action: null },
		{ label: "Quit Let's Triage Mail", enabled: true, action: 'quit', accelerator: 'Cmd+Q' },
	];
}
