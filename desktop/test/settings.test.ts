import assert from 'node:assert/strict';
import { test } from 'node:test';

import { DEFAULT_SETTINGS, isAllowedSetting, parseSettings, skippedInstallSettings, type Settings } from '../src/settings.ts';

test('parseSettings merges a valid file onto the defaults and falls back on anything else', () => {
	assert.deepEqual(parseSettings('{"appearance":"dark"}'), { appearance: 'dark', setupDone: false, notifyReply: true, notifyOffline: true });
	assert.deepEqual(
		parseSettings('{"appearance":"light","setupDone":true,"notifyReply":false,"notifyOffline":false}'),
		{ appearance: 'light', setupDone: true, notifyReply: false, notifyOffline: false },
	);
	// Unknown keys are dropped; a partly-known file keeps only the known keys.
	assert.deepEqual(parseSettings('{"appearance":"dark","bogus":7,"notifyReply":false}'), { appearance: 'dark', setupDone: false, notifyReply: false, notifyOffline: true });
	for (const bad of ['', 'not json', 'null', '[]', '{"appearance":"purple"}', '{"appearance":1}', '{}']) {
		assert.deepEqual(parseSettings(bad), DEFAULT_SETTINGS, bad);
	}
});

test('isAllowedSetting accepts only known names with the right value type', () => {
	assert.ok(isAllowedSetting('appearance', 'light'));
	assert.ok(isAllowedSetting('openAtLogin', false));
	assert.ok(isAllowedSetting('notifyReply', true));
	assert.ok(isAllowedSetting('notifyOffline', false));
	assert.equal(isAllowedSetting('appearance', 'purple'), false);
	assert.equal(isAllowedSetting('openAtLogin', 'yes'), false);
	assert.equal(isAllowedSetting('notifyReply', 'yes'), false);
	assert.equal(isAllowedSetting('setupDone', true), false, 'main writes setupDone, never the page');
	assert.equal(isAllowedSetting('theme', 'dark'), false);
	assert.equal(isAllowedSetting('__proto__', true), false);
});

test('a silent-skip install marks setup done and turns both switches off; they survive a relaunch', () => {
	const installed: Settings = { appearance: 'system', setupDone: false, notifyReply: true, notifyOffline: true };
	const skipped = skippedInstallSettings(installed);
	assert.deepEqual(skipped, { appearance: 'system', setupDone: true, notifyReply: false, notifyOffline: false });
	// Relaunch reads the saved file back: the switches stay false, they do not snap to the true default.
	const reread = parseSettings(JSON.stringify(skipped));
	assert.equal(reread.notifyReply, false, 'an opted-out/skipped install must stay silent after relaunch');
	assert.equal(reread.notifyOffline, false);
	// The helper keeps the rest of the settings (an already-chosen appearance).
	assert.equal(skippedInstallSettings({ appearance: 'dark', setupDone: false, notifyReply: true, notifyOffline: true }).appearance, 'dark');
});
