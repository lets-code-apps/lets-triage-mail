// Unit tests for the design layout rules. Every measurement here is a hand-written number (the same
// values the plan measured in the real page), so `node --test test/shots-layout.test.ts` proves the
// assertions and their violation messages with no GUI. `render.ts` runs the identical `checkLayout`
// against the eight real states, so these cases and the live pass agree by construction.
import assert from 'node:assert/strict';
import test from 'node:test';

import { checkLayout, checkPopupLayout, KNOWN_BELOW_44 } from './shots/layout.ts';
import type { Measurements, PopupMeasure, PopupCell } from './shots/layout.ts';

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

// --- the popup counts strip ---------------------------------------------------------------
// A good strip: three equal 116px-wide BUTTON cells at least 44 tall, each named with its count and the
// action, dot and arrow hidden, and a non-empty note. Mirrors the numbers MEASURE_POPUP reads in the page.
function goodPopup(): PopupMeasure {
	const cell = (ariaLabel: string): PopupCell => ({
		tag: 'BUTTON',
		width: 116,
		height: 84,
		ariaLabel,
		dotHidden: true,
		iconHidden: true,
	});
	return {
		cells: [cell('needs reply, 4, open Apple Mail'), cell('fyi, 6, open Apple Mail'), cell('done, 12, open Apple Mail')],
		noteFilled: true,
	};
}

test('a good popup strip passes with no violations', () => {
	assert.deepEqual(checkPopupLayout(goodPopup()), []);
});

test('a strip that is not three cells is reported and stops there', () => {
	const m = goodPopup();
	m.cells = m.cells.slice(0, 2);
	const violations = checkPopupLayout(m);
	assert.deepEqual(violations, ['popup strip must have 3 cells, saw 2']);
});

test('a cell that is not a button breaks keyboard reachability', () => {
	const m = goodPopup();
	m.cells[1] = { ...m.cells[1], tag: 'DIV' };
	assert.ok(checkPopupLayout(m).some((v) => v.includes('cell 1 is <div>')));
});

test('a cell under 44 tall misses the touch target', () => {
	const m = goodPopup();
	m.cells[0] = { ...m.cells[0], height: 40 };
	assert.ok(checkPopupLayout(m).some((v) => v.includes('cell 0 height 40 below the 44px touch target')));
});

test('an accessible name that omits the count is a violation', () => {
	const m = goodPopup();
	m.cells[2] = { ...m.cells[2], ariaLabel: 'done, open Apple Mail' }; // the number is gone
	assert.ok(checkPopupLayout(m).some((v) => v.includes('cell 2 aria-label must name the count')));
});

test('cells that are not equal width are reported', () => {
	const m = goodPopup();
	m.cells[2] = { ...m.cells[2], width: 90 };
	assert.ok(checkPopupLayout(m).some((v) => v.includes('cells not equal width')));
});

test('an empty note line is a violation', () => {
	const m = goodPopup();
	m.noteFilled = false;
	assert.ok(checkPopupLayout(m).some((v) => v.includes('the strip note line is empty')));
});
