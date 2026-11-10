import assert from 'node:assert/strict';
import { test } from 'node:test';

import { DEFAULT_SETTINGS, isAllowedSetting, parseSettings } from '../src/settings.ts';

test('parseSettings reads a valid file and falls back on anything else', () => {
	assert.deepEqual(parseSettings('{"appearance":"dark"}'), { appearance: 'dark' });
	for (const bad of ['', 'not json', 'null', '[]', '{"appearance":"purple"}', '{"appearance":1}', '{}']) {
		assert.deepEqual(parseSettings(bad), DEFAULT_SETTINGS, bad);
	}
});

test('isAllowedSetting accepts only known names with the right value type', () => {
	assert.ok(isAllowedSetting('appearance', 'light'));
	assert.ok(isAllowedSetting('openAtLogin', false));
	assert.equal(isAllowedSetting('appearance', 'purple'), false);
	assert.equal(isAllowedSetting('openAtLogin', 'yes'), false);
	assert.equal(isAllowedSetting('theme', 'dark'), false);
	assert.equal(isAllowedSetting('__proto__', true), false);
});
