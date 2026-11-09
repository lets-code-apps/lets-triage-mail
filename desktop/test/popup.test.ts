import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
	HIDE_GRACE_MS,
	POPUP_FALLBACK_HEIGHT,
	fitSize,
	popupPosition,
	PopupController,
	type Bounds,
	type PopupAnchor,
} from '../src/popup.ts';

// The macOS work area: the screen below a 24 px menu bar. Four displays, because the popup must
// follow the display the user clicked — and displays left of / above the primary have negative x/y.
const PRIMARY = { x: 0, y: 24, width: 1920, height: 1056 };
const RIGHT_OF = { x: 1920, y: 24, width: 2560, height: 1416 }; // secondary, non-zero x
const LEFT_OF = { x: -1920, y: 24, width: 1920, height: 1056 }; // secondary, negative x
const ABOVE = { x: 0, y: -1056, width: 1920, height: 1056 }; // secondary, negative y

const WORK = PRIMARY;
const TRAY = { x: 900, y: 4, width: 18, height: 18 };
const SIZE = { width: 360, height: 260 };
const ANCHOR: PopupAnchor = { tray: TRAY, workArea: WORK };

// A stand-in for a real BrowserWindow: three counters plus a log of what the controller did.
class FakeWindow {
	visible = false;
	shown = 0;
	hid = 0;
	bounds: Bounds[] = [];
	events: Array<'setBounds' | 'show'> = [];

	isVisible() {
		return this.visible;
	}

	show() {
		this.visible = true;
		this.shown += 1;
		this.events.push('show');
	}

	hide() {
		this.visible = false;
		this.hid += 1;
	}

	setBounds(bounds: Bounds) {
		this.bounds.push({ ...bounds });
		this.events.push('setBounds');
	}
}

// A fresh fake plus the controller that will use it; the clock is pinned at 0 unless a test moves it.
const opened = (anchor: () => PopupAnchor = () => ANCHOR): { popup: PopupController; win: FakeWindow } => {
	const win = new FakeWindow();
	const popup = new PopupController(() => win, anchor, () => 0);
	return { popup, win };
};

const makeFake = (): FakeWindow => new FakeWindow();

test('the popup is centred under the icon and below the menu bar', () => {
	const at = popupPosition(TRAY, WORK, SIZE);
	// Centred: 909 - 180 = 729. Below the 18 px icon with a 4 px gap: 4 + 18 + 4.
	assert.equal(at.x, 729);
	assert.equal(at.y, 26);
});

test('the popup is clamped at the left screen edge', () => {
	const at = popupPosition({ x: 4, y: 4, width: 18, height: 18 }, WORK, SIZE);
	assert.equal(at.x, WORK.x);
});

test('the popup is clamped at the right screen edge', () => {
	const at = popupPosition({ x: 1900, y: 4, width: 18, height: 18 }, WORK, SIZE);
	assert.equal(at.x, WORK.x + WORK.width - SIZE.width);
});

test('a tall popup never runs off the bottom of the work area', () => {
	const at = popupPosition({ x: 100, y: 1040, width: 18, height: 18 }, WORK, SIZE);
	assert.ok(at.y + SIZE.height <= WORK.y + WORK.height);
});

// --- placement on secondary displays (non-zero and negative x/y) -------------------------------

test('a popup under an icon on a display right of the primary uses that display\'s coordinates', () => {
	const at = popupPosition({ x: 3000, y: 4, width: 18, height: 18 }, RIGHT_OF, SIZE);
	// Centre 3009 - 180 = 2829, unclamped: well inside a display starting at x=1920.
	assert.equal(at.x, 2829);
	assert.equal(at.y, 26);
});

test('clamps on a right-hand display use that display\'s edges, not 0', () => {
	const right = popupPosition({ x: 4470, y: 4, width: 18, height: 18 }, RIGHT_OF, SIZE);
	assert.equal(right.x, 1920 + 2560 - 360); // its right edge
	const left = popupPosition({ x: 1925, y: 4, width: 18, height: 18 }, RIGHT_OF, SIZE);
	assert.equal(left.x, 1920); // its left edge — the old code would have allowed x < 1920
});

test('a popup on a display left of the primary clamps against negative x', () => {
	const centred = popupPosition({ x: -1000, y: 4, width: 18, height: 18 }, LEFT_OF, SIZE);
	assert.equal(centred.x, -1171); // -991 - 180: unclamped, fits
	const rightEdge = popupPosition({ x: -40, y: 4, width: 18, height: 18 }, LEFT_OF, SIZE);
	assert.equal(rightEdge.x, -360); // -0 - 360: the right clamp sits on this display's right edge
});

test('a popup on a display above the primary never goes above that display\'s top', () => {
	const belowIcon = popupPosition({ x: 900, y: -1076, width: 18, height: 18 }, ABOVE, SIZE);
	assert.equal(belowIcon.y, -1054); // -1076 + 18 + 4, inside the work area
	const atTop = popupPosition({ x: 900, y: -1080, width: 18, height: 18 }, ABOVE, SIZE);
	assert.equal(atTop.y, -1056); // the lower clamp: never above workArea.y, however high the icon
});

test('placement on any secondary display keeps the whole popup inside its work area', () => {
	const cases: { tray: Bounds; area: Bounds }[] = [
		{ tray: { x: 3000, y: 4, width: 18, height: 18 }, area: RIGHT_OF },
		{ tray: { x: 4470, y: 4, width: 18, height: 18 }, area: RIGHT_OF },
		{ tray: { x: 1925, y: 4, width: 18, height: 18 }, area: RIGHT_OF },
		{ tray: { x: -1000, y: 4, width: 18, height: 18 }, area: LEFT_OF },
		{ tray: { x: -40, y: 4, width: 18, height: 18 }, area: LEFT_OF },
		{ tray: { x: 900, y: -1076, width: 18, height: 18 }, area: ABOVE },
		{ tray: { x: 900, y: -1080, width: 18, height: 18 }, area: ABOVE },
	];
	for (const { tray, area } of cases) {
		const at = popupPosition(tray, area, SIZE);
		assert.ok(at.x >= area.x, `x ${at.x} left of ${area.x}`);
		assert.ok(at.x + SIZE.width <= area.x + area.width, `x ${at.x} past the right edge`);
		assert.ok(at.y >= area.y, `y ${at.y} above the top`);
		assert.ok(at.y + SIZE.height <= area.y + area.height, `y ${at.y} past the bottom`);
	}
});

// --- the new size: fitSize ----------------------------------------------------------------------

test('the height follows the measured content, width stays 360', () => {
	assert.deepEqual(fitSize(300, PRIMARY), { width: 360, height: 300 });
});

test('a fractional height rounds up, so no scrollbar comes back', () => {
	assert.equal(fitSize(300.2, PRIMARY).height, 301);
});

test('a popup taller than its display is capped, and placement pins it to the top', () => {
	const short = { x: 0, y: 24, width: 1024, height: 600 };
	const size = fitSize(900, short);
	assert.equal(size.height, 600);
	const at = popupPosition(TRAY, short, size);
	assert.equal(at.y, short.y); // anchored as high as it can be, still fully inside
});

test('a work area narrower than 360 caps the width too', () => {
	assert.equal(fitSize(300, { x: 0, y: 24, width: 300, height: 1056 }).width, 300);
});

test('a broken measurement keeps the fallback height, still capped', () => {
	for (const bad of [0, -5, NaN, Infinity]) {
		assert.equal(fitSize(bad, PRIMARY).height, POPUP_FALLBACK_HEIGHT, `height for ${bad}`);
	}
	assert.equal(fitSize(-5, { x: 0, y: 24, width: 1024, height: 200 }).height, 200);
});

// --- the controller places and resizes ----------------------------------------------------------

test('opening places the popup first, then shows it', () => {
	const { popup, win } = opened();
	popup.trayClick(0);
	assert.deepEqual(win.events, ['setBounds', 'show']);
	assert.deepEqual(win.bounds[0], { x: 729, y: 26, width: 360, height: 260 }); // fallback size
});

test('opening on a different display re-places: the anchor is read on every open', () => {
	// This is the controller-level proof of bug 1: the popup must not stick to the first display.
	const RIGHT_TRAY = { x: 3000, y: 4, width: 18, height: 18 };
	let anchor: PopupAnchor = { tray: TRAY, workArea: PRIMARY };
	const win = new FakeWindow();
	const popup = new PopupController(() => win, () => anchor, () => 0);
	popup.trayClick(0);
	popup.handleBlur(); // macOS closes it on blur
	anchor = { tray: RIGHT_TRAY, workArea: RIGHT_OF }; // the icon now lives on the second display
	popup.trayClick(HIDE_GRACE_MS + 10); // a new gesture opens it again
	const second = win.bounds[1];
	assert.equal(second.x, 2829); // centred under the icon at x=3000, in the second display's space
	assert.ok(second.x >= RIGHT_OF.x && second.x + second.width <= RIGHT_OF.x + RIGHT_OF.width);
	assert.ok(second.y >= RIGHT_OF.y && second.y + second.height <= RIGHT_OF.y + RIGHT_OF.height);
});

test('a fresh height while visible resizes once; repeating it does not resize again', () => {
	const { popup, win } = opened();
	popup.trayClick(0);
	popup.setContentHeight(420);
	assert.equal(win.bounds.length, 2);
	assert.deepEqual(win.bounds[1], { x: 729, y: 26, width: 360, height: 420 });
	popup.setContentHeight(420);
	assert.equal(win.bounds.length, 2, 'the same height must not flicker the window');
});

test('a fresh height while hidden is only cached; the next open uses it', () => {
	const { popup, win } = opened();
	popup.trayClick(0);
	popup.handleBlur();
	popup.setContentHeight(320);
	assert.equal(win.bounds.length, 1, 'hidden: no live resize');
	popup.trayClick(HIDE_GRACE_MS + 10);
	assert.equal(win.bounds[1].height, 320, 'the cached height survives the next open');
});

test('the cap follows the current display; the cache is never clamped destructively', () => {
	let area = { x: 0, y: 24, width: 1024, height: 600 };
	const win = new FakeWindow();
	const popup = new PopupController(() => win, () => ({ tray: TRAY, workArea: area }), () => 0);
	popup.setContentHeight(900);
	popup.trayClick(0);
	assert.equal(win.bounds[0].height, 600, 'capped to the short display');
	popup.handleBlur();
	area = PRIMARY;
	popup.trayClick(HIDE_GRACE_MS + 10);
	assert.equal(win.bounds[1].height, 900, 'the same cache opens full-height on a tall display');
});

// --- existing behaviour: toggle, blur, grace period ---------------------------------------------

test('one tray click opens the popup, the next closes it', () => {
	const popup = new PopupController(makeFake, () => ANCHOR, () => 0);
	popup.trayClick(0);
	assert.equal(popup.isVisible(), true);
	popup.trayClick(5_000); // well past the grace period: a deliberate second click
	assert.equal(popup.isVisible(), false);
});

test('clicking somewhere else hides the popup (blur)', () => {
	const popup = new PopupController(makeFake, () => ANCHOR, () => 100);
	popup.trayClick(0);
	popup.handleBlur();
	assert.equal(popup.isVisible(), false);
});

test('the click that caused the blur-hide does not reopen the popup', () => {
	// macOS order: blur (we hide), then the tray click ~ms later. One gesture, one state change.
	let now = 1_000;
	const popup = new PopupController(makeFake, () => ANCHOR, () => now);
	popup.trayClick(now);
	assert.equal(popup.isVisible(), true);
	popup.handleBlur();
	assert.equal(popup.isVisible(), false);
	now += 40; // the click that lands just after the blur
	popup.trayClick(now);
	assert.equal(popup.isVisible(), false, 'no flicker back open');
	// A later click is a new gesture and opens it again.
	now += HIDE_GRACE_MS + 10;
	popup.trayClick(now);
	assert.equal(popup.isVisible(), true);
});

test('the popup is created once and only ever hidden', () => {
	let created = 0;
	const popup = new PopupController(() => {
		created += 1;
		return makeFake();
	}, () => ANCHOR, () => 0);
	popup.trayClick(0);
	popup.trayClick(2_000);
	popup.handleBlur();
	assert.equal(created, 1);
});

test('blur before the popup exists does nothing', () => {
	const popup = new PopupController(makeFake, () => ANCHOR, () => 0);
	popup.handleBlur();
	assert.equal(popup.isVisible(), false);
});
