import assert from 'node:assert/strict';
import { test } from 'node:test';

import { WindowController } from '../src/window.ts';

// A stand-in BrowserWindow: records hide/show, tracks visibility.
function fakeWindow() {
	const w = {
		visible: true,
		hideCount: 0,
		showCount: 0,
		isVisible: () => w.visible,
		hide: () => {
			w.visible = false;
			w.hideCount += 1;
		},
		show: () => {
			w.visible = true;
			w.showCount += 1;
		},
	};
	return w;
}

function fakeDock() {
	const d = {
		hidden: false,
		shown: false,
		hide: () => {
			d.hidden = true;
			d.shown = false;
		},
		show: () => {
			d.shown = true;
			d.hidden = false;
		},
	};
	return d;
}

function fresh() {
	let createdCount = 0;
	let last: ReturnType<typeof fakeWindow> | null = null;
	const dock = fakeDock();
	const controller = new WindowController(
		() => {
			createdCount += 1;
			last = fakeWindow();
			return last;
		},
		dock,
	);
	return { controller, dock, created: () => createdCount, handle: () => last as ReturnType<typeof fakeWindow> };
}

const closeEvent = () => {
	let defaultPrevented = false;
	return {
		get defaultPrevented() {
			return defaultPrevented;
		},
		preventDefault: () => {
			defaultPrevented = true;
		},
	};
};

test('close is prevented and hides the window; the dock hides with it', () => {
	const { controller, dock, handle } = fresh();
	controller.open();
	const event = closeEvent();
	controller.handleClose(event);
	assert.equal(event.defaultPrevented, true);
	assert.equal(handle().hideCount, 1);
	assert.equal(handle().visible, false);
	assert.equal(dock.hidden, true);
});

test('open shows the window and the dock', () => {
	const { controller, dock, handle } = fresh();
	controller.open();
	assert.equal(handle().showCount, 1);
	assert.equal(dock.shown, true);
});

test('close while quitting is NOT prevented (the window really goes away)', () => {
	const { controller, handle } = fresh();
	controller.open();
	controller.beginQuitting();
	const event = closeEvent();
	controller.handleClose(event);
	assert.equal(event.defaultPrevented, false);
	assert.equal(handle().hideCount, 0);
});

test('window-all-closed never quits the app', () => {
	const { controller, dock, handle } = fresh();
	controller.open();
	// Bound to `window-all-closed`: it must do nothing.
	controller.handleWindowAllClosed();
	assert.equal(handle().hideCount, 0);
	assert.equal(dock.hidden, false);
});

test('reopening reuses the SAME window (created once, never destroyed)', () => {
	const { controller, created, handle } = fresh();
	controller.open();
	const first = handle();
	controller.handleClose(closeEvent());
	controller.open();
	assert.equal(created(), 1);
	assert.equal(handle(), first);
	assert.equal(handle().visible, true);
	assert.equal(handle().hideCount, 1);
});
