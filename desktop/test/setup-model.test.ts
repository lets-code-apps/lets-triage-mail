import assert from 'node:assert/strict';
import { test } from 'node:test';

import { modelView, pickRecommended, type ModelStepData } from '../src/setup-model.ts';

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

test('the model step never claims the model answered a test question (ready = installed and saved)', () => {
	const view = modelView('ready', data({ sharedModel: false }));
	for (const text of [view.body ?? '', view.note ?? '']) {
		assert.equal(/test question|answered/i.test(text), false, `ready copy must not mention a test question: ${text}`);
	}
	assert.ok((view.note ?? '').includes('Installed'));
	assert.equal(view.nextOff, false);
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
