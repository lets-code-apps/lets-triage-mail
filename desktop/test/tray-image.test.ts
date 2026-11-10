import assert from 'node:assert/strict';
import { test } from 'node:test';

import { CANVAS, trayBitmap } from '../src/tray-image.ts';

// A 36x36 px glyph: fully opaque in the middle, transparent at its own corner.
const GLYPH = 36;
const glyph = new Uint8Array(GLYPH * GLYPH * 4);
for (let i = 0; i < GLYPH * GLYPH; i += 1) glyph[i * 4 + 3] = i === 0 ? 0 : 255;

const alphaAt = (bitmap: Buffer, x: number, y: number): number => bitmap[(y * CANVAS.width + x) * 4 + 3];

test('both versions share one canvas size', () => {
	for (const pill of [false, true]) {
		assert.equal(trayBitmap(glyph, GLYPH, GLYPH, pill).length, CANVAS.width * CANVAS.height * 4);
	}
});

test('without the pill only the glyph has alpha, centred on the canvas', () => {
	const plain = trayBitmap(glyph, GLYPH, GLYPH, false);
	assert.equal(alphaAt(plain, 0, 0), 0);
	assert.equal(alphaAt(plain, 28, 22), 255); // canvas centre is inside the glyph
	assert.equal(alphaAt(plain, 9, 22), 0); // left of the glyph (offset 10)
});

test('with the pill: ~25% fill, rounded corners, glyph kept at full alpha', () => {
	const active = trayBitmap(glyph, GLYPH, GLYPH, true);
	assert.equal(alphaAt(active, 0, 0), 0); // the rounded corner is cut away
	assert.equal(alphaAt(active, 2, 22), 64); // padding left of the glyph carries the pill
	assert.equal(alphaAt(active, 28, 22), 255); // glyph pixel preserved
	assert.equal(alphaAt(active, 10, 4), 64); // the glyph's own transparent corner shows the pill
});
