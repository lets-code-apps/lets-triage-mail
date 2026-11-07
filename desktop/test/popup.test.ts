import assert from 'node:assert/strict';
import { test } from 'node:test';

import { HIDE_GRACE_MS, PopupController, popupPosition } from '../src/popup.ts';

// The macOS work area: the screen below the 24 px menu bar.
const WORK = { x: 0, y: 24, width: 1920, height: 1056 };
const TRAY = { x: 900, y: 4, width: 18, height: 18 };
const SIZE = { width: 360, height: 260 };

// A stand-in for a real BrowserWindow: three counters, no Electron.
class FakeWindow {
	visible = false;
	shown = 0;
	hid = 0;

	isVisible() {
		return this.visible;
	}

	show() {
		this.visible = true;
		this.shown += 1;
	}

	hide() {
		this.visible = false;
		this.hid += 1;
	}
}

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

test('one tray click opens the popup, the next closes it', () => {
	const popup = new PopupController(makeFake, () => 0);
	popup.trayClick(0);
	assert.equal(popup.isVisible(), true);
	popup.trayClick(5_000); // well past the grace period: a deliberate second click
	assert.equal(popup.isVisible(), false);
});

test('clicking somewhere else hides the popup (blur)', () => {
	const popup = new PopupController(makeFake, () => 100);
	popup.trayClick(0);
	popup.handleBlur();
	assert.equal(popup.isVisible(), false);
});

test('the click that caused the blur-hide does not reopen the popup', () => {
	// macOS order: blur (we hide), then the tray click ~ms later. One gesture, one state change.
	let now = 1_000;
	const popup = new PopupController(makeFake, () => now);
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
	}, () => 0);
	popup.trayClick(0);
	popup.trayClick(2_000);
	popup.handleBlur();
	assert.equal(created, 1);
});

test('blur before the popup exists does nothing', () => {
	const popup = new PopupController(makeFake, () => 0);
	popup.handleBlur();
	assert.equal(popup.isVisible(), false);
});
