// The model step's state machine. Everything the user sees about the model comes from the engine's
// own `model detect`/`model save` answer, plus the model part of `check --json` for the ready state.
// No network call, no reading of any file: the desktop never probes a runtime or revalidates a URL.
// "Ready" is decided from the engine's own test classification (`check-report.ts` -> `modelAnswer`): when
// that line is present the step says the model answered a test question and shows the engine's line;
// when `check` reports no model result it falls back to installed-and-saved and claims nothing about a
// test.

import type { SetupStep, SetupView, Tone } from './ipc.ts';
import { recommendedModel } from './setup.ts';
import type { ModeChoice, ModelDetect } from './setup.ts';

export type ModelStepState = 'shared' | 'detecting' | 'found' | 'not-found' | 'downloading' | 'ready' | 'failed';

export interface ModelStepData {
	runtimeName: string; // 'Ollama' | 'LM Studio'
	url: string; // the local runtime URL the engine reported
	model: string; // chosen / recommended / pulling / ready model id
	usable?: string[]; // installed models the engine lists as usable
	memGb?: number; // the Mac's memory, in whole GB, from main
	sharedModel?: boolean; // true when the shared model was set up by another app
	setBy?: string; // that other app's name (for the shared note)
	pull?: { model: string; done: number; gb: number; pct: number } | null;
	error?: string; // the engine's one-sentence reason, for the failed state
	checkAnswer?: string; // the engine's `check --json` test-classification line, when it worked (check-report.ts modelAnswer)
}

// The address field, present in every state except `shared` (a shared model was set up elsewhere).
const ADDRESS_FIELD = {
	label: 'runtime address',
	kind: 'text',
	placeholder: 'http://localhost:11434',
	helper: 'Another port or runtime? Type its address.',
} as const;

// The step's title and description are fixed for every state.
const TITLE = 'model';
const TEXT = 'Triage mail judges each new message with a model that runs on this Mac.';

// The model recommended for the installed set: the memory-sized one if it is installed, else the
// first installed model that is not an embedding model, else the first installed model. An embedding
// model's id carries "embed" and is never offered as the working model.
export function pickRecommended(usable: string[], memGb: number): string {
	if (usable.length === 0) return '';
	const sized = recommendedModel(memGb * 1024 ** 3).model;
	if (usable.includes(sized)) return sized;
	const notEmbedding = usable.find((id) => !id.includes('embed'));
	return notEmbedding ?? usable[0];
}

// Turn one model-step state into the exact view the page paints. All strings are engine-derived or
// fixed copy; the page never formats or colours them.
export function modelView(state: ModelStepState, data: ModelStepData): SetupView {
	const base: SetupView = { step: 'model', index: 2, title: TITLE, text: TEXT };

	switch (state) {
		case 'shared':
			return {
				...base,
				pill: 'using the model from rag docs',
				tone: 'positive',
				body: `${data.model} in ${data.runtimeName} at ${data.url}, set up when you installed rag docs.`,
				note: `Already set up for you on this Mac${data.setBy ? ` (by ${data.setBy})` : ''}.`,
				actions: [{ label: 'use another model', variant: 'secondary' }],
				nextLabel: 'Continue',
				nextOff: false,
			};
		case 'detecting':
			return {
				...base,
				pill: 'looking for a runtime',
				tone: 'muted',
				body: `Checking ${data.runtimeName} at ${data.url}.`,
				note: 'The model runs on this Mac, so nothing you give it leaves the computer.',
				fields: [{ ...ADDRESS_FIELD }],
				nextOff: true,
			};
		case 'found': {
			const sized = recommendedModel((data.memGb ?? 0) * 1024 ** 3);
			const usable = data.usable ?? [];
			// A runtime that answers but has nothing usable (the design's open proposals, as the plan
			// decides): the primary action is the download that fits this Mac, and Continue waits for a
			// model to be chosen.
			if (usable.length === 0) {
				return {
					...base,
					pill: `${data.runtimeName} found`,
					tone: 'attention',
					body: `${data.runtimeName} is running at ${data.url}, with no model triage mail can use yet.`,
					note: `Downloading ${sized.model} (${sized.sizeGb} GB) fits this Mac's ${data.memGb ?? 0} GB of memory.`,
					actions: [{ label: `download ${sized.model}`, glyph: 'arrow-down', variant: 'primary' }],
					fields: [{ ...ADDRESS_FIELD }],
					nextLabel: 'Continue',
					nextOff: true,
				};
			}
			const selected = pickRecommended(usable, data.memGb ?? 0);
			return {
				...base,
				pill: `${data.runtimeName} found`,
				tone: 'positive',
				body: `${data.runtimeName} is running at ${data.url} with ${usable.length} models. Triage mail picked the one that fits this Mac.`,
				note: `Recommended for this Mac's ${data.memGb ?? 0} GB of memory: ${selected}, ${sized.sizeGb} GB.`,
				options: usable.map((id) => ({ value: id, label: id })),
				selected,
				fields: [{ ...ADDRESS_FIELD }],
				nextLabel: 'Continue',
				nextOff: false,
			};
		}
		case 'not-found':
			return {
				...base,
				pill: 'no runtime found',
				tone: 'attention',
				body: 'Triage mail needs a model runtime on this Mac. Ollama is free and runs models locally, so nothing leaves the computer.',
				actions: [
					{ label: 'install Ollama', glyph: 'arrow-up-right', variant: 'primary' },
					{ label: 'check again', variant: 'secondary' },
				],
				fields: [{ ...ADDRESS_FIELD }],
				nextLabel: 'set up later',
				nextVariant: 'secondary',
				nextOff: false,
			};
		case 'downloading': {
			const pull = data.pull ?? { model: data.model, done: 0, gb: 0, pct: 0 };
			return {
				...base,
				pill: 'downloading model',
				body: `${data.runtimeName} is running. Triage mail is downloading ${pull.model} through it.`,
				progress: { label: `downloading ${pull.model}`, value: pull.done, max: pull.gb, pct: pull.pct },
				actions: [{ label: 'cancel download' }],
				fields: [{ ...ADDRESS_FIELD }],
				nextOff: false,
			};
		}
		case 'ready': {
			const proof = data.checkAnswer ?? '';
			return {
				...base,
				pill: 'ready',
				tone: 'positive',
				body: proof
					? `${data.model} runs in ${data.runtimeName} on this Mac and answered a test question.`
					: `${data.model} runs in ${data.runtimeName} on this Mac and is ready for triage mail.`,
				note: data.sharedModel ? 'The same model another app already uses on this Mac.' : proof || 'Installed and saved on this Mac.',
				fields: [{ ...ADDRESS_FIELD }],
				nextLabel: 'Continue',
				nextOff: false,
			};
		}
		case 'failed':
			return {
				...base,
				pill: 'not ready',
				tone: 'danger',
				body: data.error ?? 'The model could not be set up.',
				actions: [{ label: 'try again' }],
				fields: [{ ...ADDRESS_FIELD }],
				nextLabel: 'Continue',
				nextOff: false,
			};
	}
}

// --- the accounts, mode and digest steps -------------------------------------------
//
// Same split as the model step: fixed design copy here, engine-derived facts from main, and the page
// paints strings and control states only.

// The accounts step has nothing to pre-fill: Continue waits for one account to sign in.
const ACCOUNTS_TEXT = 'Add the mail accounts to triage. Triage mail signs in over IMAP and watches each inbox.';
const ACCOUNTS_MUTED = "The password stays in this Mac's keychain. Continue works once one account signs in.";

export type AccountsStepData = { accounts: string[]; missing: boolean; error: string | null };

// "work signed in, and 2 more" (the plan's note: the first inbox named, the rest counted).
const signedInNote = (accounts: string[]): string =>
	accounts.length === 0 ? '' : `${accounts[0]} signed in${accounts.length > 1 ? `, and ${accounts.length - 1} more` : ''}`;

export function accountsView(data: AccountsStepData): SetupView {
	const base: SetupView = { step: 'accounts', index: 0, title: 'accounts', text: ACCOUNTS_TEXT };
	const note = signedInNote(data.accounts);
	return {
		...base,
		note: note === '' ? undefined : note,
		muted: ACCOUNTS_MUTED,
		actions: data.accounts.length > 0 ? [{ label: 'add another account', glyph: 'plus', variant: 'secondary' }] : undefined,
		nextLabel: 'continue',
		// Continue is disabled until one account has passed (the plan's rule, read off the config).
		nextOff: data.accounts.length === 0,
	};
}

// The mode step's two choices, in the design's order, "classify and sort" pre-selected.
export const MODE_OPTIONS: { value: string; label: string }[] = [
	{ value: 'classify-only', label: 'classify only' },
	{ value: 'classify-and-sort', label: 'classify and sort' },
];

export type ModeStepData = { choice: ModeChoice };

export function modeView(data: ModeStepData): SetupView {
	return {
		step: 'mode',
		index: 1,
		title: 'mode',
		text: 'What triage mail does with new mail in your inbox.',
		body: 'The model judges each new message in your inbox, then triage mail acts on it: the actions are set per folder in accounts.',
		muted: 'Recommended. Classify only shows the results in the digest and changes nothing on the server.',
		options: MODE_OPTIONS.map((option) => ({ ...option })),
		selected: data.choice,
		nextLabel: 'continue',
		nextOff: false,
	};
}

// The digest and notifications step: one switch, one field, and the two notification switches (the
// board's rows, so a skip-setup install can turn them on from settings later).
export type DigestStepData = { enabled: boolean; time: string; accounts: string[]; notifyReply: boolean; notifyOffline: boolean };

export function digestView(data: DigestStepData): SetupView {
	const first = data.accounts[0];
	const digestNote =
		first === undefined
			? undefined
			: `One email a day to ${first} with what needs a reply${data.accounts.length > 1 ? `, and ${data.accounts.length - 1} more` : ''}`;
	return {
		step: 'digest',
		index: 3,
		title: 'digest and notifications',
		text: 'How triage mail tells you what came in.',
		muted: 'macOS asks once whether triage mail may send notifications.',
		note: digestNote,
		fields: [
			{ label: 'daily digest', kind: 'switch', checked: data.enabled, note: digestNote },
			{ label: 'send at', kind: 'text', value: data.time, glyph: 'clock' },
			{ label: 'mail that needs a reply', kind: 'switch', checked: data.notifyReply, note: 'One notification per message, nothing for fyi or done.' },
			{ label: 'connection problems', kind: 'switch', checked: data.notifyOffline, note: 'Only when an account stays offline for 5 minutes.' },
		],
		nextLabel: 'start triage',
		nextOff: false,
	};
}

// The step names in order, and one dispatch: main holds the step number, the page asks for one view.
export const STEP_ORDER: SetupStep[] = ['accounts', 'mode', 'model', 'digest'];

export type StepInput =
	| { step: 'accounts'; data: AccountsStepData }
	| { step: 'mode'; data: ModeStepData }
	| { step: 'model'; state: ModelStepState; data: ModelStepData }
	| { step: 'digest'; data: DigestStepData };

export function stepView(input: StepInput): SetupView {
	switch (input.step) {
		case 'accounts':
			return accountsView(input.data);
		case 'mode':
			return modeView(input.data);
		case 'model':
			return modelView(input.state, input.data);
		case 'digest':
			return digestView(input.data);
	}
}

// The runtime display names (`ollama`/`lmstudio` are the engine's; anything else is "a local runtime",
// so a custom server's url is what the user reads, not a label we invented).
export const RUNTIME_NAMES: Record<string, string> = { ollama: 'Ollama', lmstudio: 'LM Studio' };
const runtimeName = (runtime: string): string => RUNTIME_NAMES[runtime] ?? 'a local runtime';

// Turn one `model detect --json` answer into the step's state. A shared block only wins when it
// answers (a shared file whose server is off falls through to the probes, per the plan's open risk);
// a runtime that answers with nothing usable stays `found` and offers the sized download.
export function stepFromDetect(detect: ModelDetect, memGb: number): { state: ModelStepState; data: ModelStepData } {
	const shared = detect.shared;
	if (shared !== null && shared.reachable && shared.model !== '') {
		return {
			state: 'shared',
			data: { runtimeName: runtimeName(shared.runtime), url: shared.url, model: shared.model, setBy: shared.setBy, sharedModel: true, memGb },
		};
	}
	const answering = detect.runtimes.find((probe) => probe.reachable);
	if (answering === undefined) {
		return {
			state: 'not-found',
			data: { runtimeName: 'Ollama', url: detect.runtimes[0]?.url ?? 'http://localhost:11434', model: recommendedModel(memGb * 1024 ** 3).model, memGb },
		};
	}
	return {
		state: 'found',
		data: { runtimeName: runtimeName(answering.runtime), url: answering.url, model: pickRecommended(answering.usable, memGb), usable: answering.usable, memGb },
	};
}

// The four-tone allow-list, spelled once so a fifth tone is a compile error, not a silent style.
export const TONES: readonly Tone[] = ['positive', 'attention', 'danger', 'muted'];
