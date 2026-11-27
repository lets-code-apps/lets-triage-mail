// The model step's state machine. Everything the user sees about the model comes from the engine's
// own `model detect`/`model save` answer, turned into a ready-to-print `SetupView` here. No network
// call, no reading of any file: the desktop never probes a runtime or revalidates a URL. "Ready"
// means installed-and-saved, decided from `model detect` + a successful save — there is no `model
// test` and the copy never claims the model "answered a test question" (health, if ever shown,
// reuses the `check --json` items on the settings screen).

import type { SetupView, Tone } from './ipc.ts';
import { recommendedModel } from './setup.ts';

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
			const selected = pickRecommended(data.usable ?? [], data.memGb ?? 0);
			return {
				...base,
				pill: `${data.runtimeName} found`,
				tone: 'positive',
				body: `${data.runtimeName} is running at ${data.url} with ${(data.usable ?? []).length} models. Triage mail picked the one that fits this Mac.`,
				note: `Recommended for this Mac's ${data.memGb ?? 0} GB of memory: ${selected}, ${sized.sizeGb} GB.`,
				options: (data.usable ?? []).map((id) => ({ value: id, label: id })),
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
		case 'ready':
			return {
				...base,
				pill: 'ready',
				tone: 'positive',
				body: `${data.model} runs in ${data.runtimeName} on this Mac and is ready for triage mail.`,
				note: data.sharedModel ? `The same model another app already uses on this Mac.` : 'Installed and saved on this Mac.',
				fields: [{ ...ADDRESS_FIELD }],
				nextLabel: 'Continue',
				nextOff: false,
			};
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

// The four-tone allow-list, spelled once so a fifth tone is a compile error, not a silent style.
export const TONES: readonly Tone[] = ['positive', 'attention', 'danger', 'muted'];
