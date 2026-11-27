import assert from 'node:assert/strict';
import { test } from 'node:test';

import { modelView, pickRecommended, stepFromDetect, stepView, type ModelStepData } from '../src/setup-model.ts';

const data = (over: Partial<ModelStepData> = {}): ModelStepData => ({
	runtimeName: 'Ollama',
	url: 'http://127.0.0.1:11434',
	model: 'qwen3:8b',
	...over,
});

test('a shared model (set up by another app) shows no runtime address field', () => {
	const view = modelView('shared', data({ model: 'qwen3:14b', setBy: 'rag docs', sharedModel: true }));
	assert.equal(view.step, 'model');
	assert.equal(view.tone, 'positive');
	assert.ok((view.body ?? '').includes('set up when you installed rag docs'));
	assert.equal(view.fields, undefined, 'a shared model has no address field');
	assert.ok((view.actions ?? []).some((a) => a.label === 'use another model'));
});

test('while detecting, Continue is disabled', () => {
	const view = modelView('detecting', data());
	assert.equal(view.nextOff, true);
	assert.equal(view.tone, 'muted');
	assert.equal(view.actions, undefined);
});

test('no runtime found offers install Ollama, re-check, and a secondary set-up-later continue', () => {
	const view = modelView('not-found', data());
	assert.equal(view.tone, 'attention');
	assert.equal(view.nextLabel, 'set up later');
	assert.equal(view.nextVariant, 'secondary');
	const labels = (view.actions ?? []).map((a) => a.label);
	assert.ok(labels.includes('install Ollama'));
	assert.ok(labels.includes('check again'));
});

test('found offers the usable models and recommends for the Mac', () => {
	const view = modelView('found', data({ usable: ['qwen3:4b', 'nomic-embed-text', 'qwen3:8b'], memGb: 24 }));
	assert.deepEqual(view.options, [
		{ value: 'qwen3:4b', label: 'qwen3:4b' },
		{ value: 'nomic-embed-text', label: 'nomic-embed-text' },
		{ value: 'qwen3:8b', label: 'qwen3:8b' },
	]);
	assert.equal(view.selected, 'qwen3:8b');
	assert.ok((view.note ?? '').includes('Recommended for this Mac'));
});

test('a download reports a progress bar and still allows Continue', () => {
	const view = modelView('downloading', data({ pull: { model: 'qwen3:8b', done: 4, gb: 5, pct: 80 } }));
	assert.equal(view.progress?.label, 'downloading qwen3:8b');
	assert.deepEqual({ value: view.progress?.value, max: view.progress?.max, pct: view.progress?.pct }, { value: 4, max: 5, pct: 80 });
	assert.ok((view.actions ?? []).some((a) => a.label === 'cancel download'));
});

const ANSWERED = 'test classification worked in 1.4 s (judged "junk" with confidence 0.91)';

test('ready comes from the model part of check --json: the step says the model answered, and quotes the engine line', () => {
	const view = modelView('ready', data({ checkAnswer: ANSWERED }));
	assert.equal(view.pill, 'ready');
	assert.equal(view.tone, 'positive');
	assert.ok((view.body ?? '').includes('answered a test question'));
	assert.equal(view.note, ANSWERED, 'the note is the engine line, passed through unchanged');
	assert.equal(view.nextOff, false);
});

test('ready with no model result in check falls back to installed and saved, and claims no test question', () => {
	const view = modelView('ready', data());
	assert.ok((view.note ?? '').includes('Installed and saved'));
	for (const text of [view.body ?? '', view.note ?? '']) {
		assert.equal(/test question|answered/i.test(text), false, `ready copy must not claim a test without the engine line: ${text}`);
	}
});

test('a shared model keeps the shared note, whether or not check confirmed it', () => {
	const view = modelView('ready', data({ sharedModel: true, setBy: 'rag docs', checkAnswer: ANSWERED }));
	assert.ok((view.body ?? '').includes('answered a test question'));
	assert.ok((view.note ?? '').includes('another app'));
});

test('a failed model step shows the engine sentence and a try again', () => {
	const view = modelView('failed', data({ error: 'Ollama refused the download' }));
	assert.equal(view.tone, 'danger');
	assert.equal(view.body, 'Ollama refused the download');
	assert.ok((view.actions ?? []).some((a) => a.label === 'try again'));
});

test('pickRecommended prefers the size-matched model, then a non-embedding model, never an embedding first', () => {
	assert.equal(pickRecommended(['qwen3:4b', 'nomic-embed-text'], 8), 'qwen3:4b');
	assert.equal(pickRecommended(['nomic-embed-text', 'qwen3:8b'], 16), 'qwen3:8b');
	assert.equal(pickRecommended(['nomic-embed-text', 'llama3:8b'], 16), 'llama3:8b');
	assert.equal(pickRecommended(['nomic-embed-text'], 8), 'nomic-embed-text');
	assert.equal(pickRecommended([], 16), '');
});

test('found with nothing usable offers the sized download and holds Continue', () => {
	const view = modelView('found', data({ usable: [], memGb: 24 }));
	assert.equal(view.tone, 'attention');
	const actions = view.actions ?? [];
	assert.ok(actions.some((a) => a.label === 'download qwen3:8b'));
	assert.equal(view.nextOff, true, 'Continue waits until a model is chosen');
	assert.equal(view.selected, undefined);
});

test('the accounts step: nothing to pre-fill, so Continue waits for one signed-in account', () => {
	const view = stepView({ step: 'accounts', data: { accounts: [], missing: true, error: null } });
	assert.equal(view.step, 'accounts');
	assert.equal(view.title, 'accounts');
	assert.ok((view.text ?? '').includes('signs in over IMAP'));
	assert.equal(view.nextOff, true);
	assert.ok((view.muted ?? '').includes('keychain'));
});

test('the accounts step with accounts: named note, add another account, Continue on', () => {
	const view = stepView({ step: 'accounts', data: { accounts: ['work', 'home', 'ex'], missing: false, error: null } });
	assert.equal(view.note, 'work signed in, and 2 more');
	assert.ok((view.actions ?? []).some((a) => a.label === 'add another account'));
	assert.equal(view.nextOff, false);
});

test('the mode step pre-selects classify and sort, in the design option order', () => {
	const view = stepView({ step: 'mode', data: { choice: 'classify-and-sort' } });
	assert.deepEqual((view.options ?? []).map((o) => o.value), ['classify-only', 'classify-and-sort']);
	assert.equal(view.selected, 'classify-and-sort');
	assert.ok((view.muted ?? '').includes('Classify only shows the results in the digest'));
	assert.equal(view.nextOff, false);
});

test('the digest step carries both switches as rows, with send at and the macOS note', () => {
	const view = stepView({ step: 'digest', data: { enabled: true, time: '08:00', accounts: ['alex@example.com'], notifyReply: true, notifyOffline: false } });
	assert.equal(view.nextLabel, 'start triage');
	const labels = (view.fields ?? []).map((f) => f.label);
	assert.deepEqual(labels, ['daily digest', 'send at', 'mail that needs a reply', 'connection problems']);
	const digestField = view.fields?.[0];
	assert.equal(digestField?.kind, 'switch');
	assert.ok((digestField?.note ?? '').includes('One email a day to alex@example.com'));
	assert.equal(view.fields?.[3]?.checked, false, 'a switched-off notification row reads off');
	assert.ok((view.muted ?? '').includes('macOS asks once'));
});

test('stepFromDetect: a shared block only wins when its server answers', () => {
	const shared = { runtime: 'ollama', url: 'http://127.0.0.1:11434', model: 'qwen3:14b', setBy: 'rag docs', reachable: true };
	const fromShared = stepFromDetect({ ok: true, runtimes: [], shared, error: null }, 24);
	assert.equal(fromShared.state, 'shared');
	assert.equal(fromShared.data.model, 'qwen3:14b');
	assert.equal(fromShared.data.setBy, 'rag docs');

	// The shared file is there but its server is off now: fall through to the probes.
	const off = stepFromDetect({ ok: true, runtimes: [{ runtime: 'ollama', url: 'http://127.0.0.1:11434', reachable: false, usable: [], error: 'connection refused' }], shared: { ...shared, reachable: false }, error: null }, 24);
	assert.equal(off.state, 'not-found');
});

test('stepFromDetect: a reachable runtime becomes found, none becomes not-found', () => {
	const found = stepFromDetect({ ok: true, runtimes: [{ runtime: 'ollama', url: 'http://127.0.0.1:11434', reachable: true, usable: ['qwen3:8b'], error: null }], shared: null, error: null }, 24);
	assert.equal(found.state, 'found');
	assert.equal(found.data.runtimeName, 'Ollama');
	const none = stepFromDetect({ ok: true, runtimes: [{ runtime: 'lmstudio', url: 'http://127.0.0.1:1234', reachable: false, usable: [], error: 'unreachable' }], shared: null, error: null }, 8);
	assert.equal(none.state, 'not-found');
});

test('stepView dispatches the model step through modelView', () => {
	const view = stepView({ step: 'model', state: 'ready', data: data({ checkAnswer: ANSWERED }) });
	assert.equal(view.title, 'model');
	assert.ok((view.body ?? '').includes('answered a test question'));
});
