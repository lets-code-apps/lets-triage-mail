// Unit tests for the design layout rules. Every measurement here is a hand-written number (the same
// values the plan measured in the real page), so `node --test test/shots-layout.test.ts` proves the
// assertions and their violation messages with no GUI. `render.ts` runs the identical `checkLayout`
// against the eight real states, so these cases and the live pass agree by construction.
import assert from 'node:assert/strict';
import test from 'node:test';

import { checkLayout, KNOWN_BELOW_44 } from './shots/layout.ts';
import type { Measurements } from './shots/layout.ts';

// A state that satisfies every rule: the title centred with 132px of clear on one line, the emblem
// at the bar's right end (24px inset), five aligned settings rows, and controls at or above their
// floors. Numbers match the plan's measurements for a 1100 and a 360 window.
function good(width: number): Measurements {
	const srowLeft = width === 1100 ? 260 : 45;
	const srowRight = width === 1100 ? 940 : 425;
	return {
		width,
		title: { left: width / 2 - 45, right: width / 2 + 45, width: 90, height: 20, centerX: width / 2, centerY: 32, scrollWidth: 90, clientWidth: 90 },
		emblem: { left: width - 56, right: width - 24, width: 32, height: 32, centerX: width - 40, centerY: 32 },
		bar: { right: width, centerY: 32 },
		srows: [0, 1, 2, 3, 4].map((i) => ({ left: srowLeft, right: srowRight, top: 100 + i * 90, bottom: 100 + i * 90 + 75 })),
		controls: [
			{ role: 'nav', width: 176, height: 42 },
			{ role: 'seg', width: 80, height: 38 },
			{ role: 'btn', width: 120, height: 44 },
			{ role: 'other', width: 300, height: 129 },
			{ role: 'switch', width: 44, height: 24 },
		],
		switchRowHeight: 75,
	};
}

test('a good window passes with no violations (both sections, both widths)', () => {
	assert.deepEqual(checkLayout('accounts', good(1100)), []);
	assert.deepEqual(checkLayout('settings', good(1100)), []);
	assert.deepEqual(checkLayout('settings', good(360)), []);
});

test('a title shifted 3px is reported as not centred', () => {
	const m = good(1100);
	m.title.centerX += 3;
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('title not centred')), JSON.stringify(violations));
});

test('a title pushed to left: 120 breaks the 132px-clear rule', () => {
	const m = good(1100);
	m.title.left = 120;
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('132px clear')), JSON.stringify(violations));
});

test('an emblem 10px left of the bar end is reported', () => {
	const m = good(1100);
	m.emblem.right -= 10;
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('emblem not at the right end')), JSON.stringify(violations));
});

test('one settings row offset by 2px breaks the alignment rule', () => {
	const m = good(1100);
	m.srows[2].left += 2;
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('settings rows not aligned')), JSON.stringify(violations));
});

test('a 40px-tall .btn is a violation', () => {
	const m = good(1100);
	m.controls.push({ role: 'btn', width: 44, height: 40 });
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('.btn height 40')), JSON.stringify(violations));
});

test('the ratchet: a .seg button may not fall below its floor (38)', () => {
	const atFloor = good(1100);
	atFloor.controls.push({ role: 'seg', width: 80, height: 38 });
	assert.equal(KNOWN_BELOW_44.seg, 38, 'the committed floor for .seg is the measured 38');
	assert.deepEqual(checkLayout('settings', atFloor), [], 'exactly at the floor is allowed');

	const below = good(1100);
	below.controls.push({ role: 'seg', width: 80, height: 37 });
	const violations = checkLayout('settings', below);
	assert.ok(violations.some((v) => v.includes('.seg button height 37 below floor 38')), JSON.stringify(violations));
});

test('a title wider than its box (scrollWidth > clientWidth) is a violation', () => {
	const m = good(1100);
	m.title.scrollWidth = m.title.clientWidth + 1;
	const violations = checkLayout('settings', m);
	assert.ok(violations.some((v) => v.includes('overflows its box')), JSON.stringify(violations));
});
