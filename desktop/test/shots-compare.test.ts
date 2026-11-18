// Unit tests for the pixel comparison. They build raw BGRA buffers by hand (no Electron, no image
// files, no PNG decode), so `node --test test/shots-compare.test.ts` proves the whole diff algorithm
// and the threshold on the exact rules the plan set. This mirrors how popup-model.test.ts proves the
// popup's text without a page.
import assert from 'node:assert/strict';
import test from 'node:test';

import { PIXEL_DELTA, MAX_CHANGED_RATIO, compareShots, diffBitmaps, makeDiffBitmap } from './shots/compare.ts';

// A tiny image as a solid colour: `data` is BGRA, so a "blue-ish" pixel is [b, g, r, a].
const solid = (w: number, h: number, colour: [number, number, number]): Uint8Array => {
	const data = new Uint8Array(w * h * 4);
	for (let i = 0; i < w * h; i += 1) {
		data[i * 4] = colour[0];
		data[i * 4 + 1] = colour[1];
		data[i * 4 + 2] = colour[2];
		data[i * 4 + 3] = 255;
	}
	return data;
};

test('identical bitmaps diff to zero', () => {
	const w = 20;
	const h = 20;
	const a = solid(w, h, [10, 200, 90]);
	const b = solid(w, h, [10, 200, 90]);
	const { changed, ratio } = diffBitmaps(a, b, w, h);
	assert.equal(changed, 0);
	assert.equal(ratio, 0);
	assert.equal(compareShots({ width: w, height: h, data: a }, { width: w, height: h, data: b }).kind, 'ok');
});

test('a single channel off by 5 is within the delta and not counted', () => {
	const w = 20;
	const h = 20;
	const a = solid(w, h, [10, 200, 90]);
	const b = a.slice();
	b[2] = 95; // red channel 90 -> 95 on the first pixel; gap 5 <= PIXEL_DELTA
	assert.equal(PIXEL_DELTA, 16, 'the plan starts with a delta of 16');
	const { changed } = diffBitmaps(a, b, w, h);
	assert.equal(changed, 0, 'a 5-unit jitter is antialiasing, not a change');
});

test('a 40x40 block of real change is over the ratio and reported as changed', () => {
	const w = 100;
	const h = 100;
	const a = solid(w, h, [0, 0, 0]);
	const b = a.slice();
	for (let y = 0; y < 40; y += 1) {
		for (let x = 0; x < 40; x += 1) {
			const p = (y * w + x) * 4;
			b[p + 2] = 255; // flip the red channel of a 40x40 block -> 1600 changed pixels
		}
	}
	assert.ok(compareShots({ width: w, height: h, data: a }, { width: w, height: h, data: b }, { maxRatio: MAX_CHANGED_RATIO }).kind === 'changed');
	const shot = compareShots({ width: w, height: h, data: a }, { width: w, height: h, data: b });
	assert.equal(shot.changed, 1600);
	assert.equal(shot.ratio, 0.16);
	assert.ok(shot.ratio > MAX_CHANGED_RATIO, '0.16 is far above the 0.1% budget');
});

test('a size mismatch is reported as SIZE, not a crash', () => {
	const a = solid(100, 76, [0, 0, 0]);
	const b = solid(120, 76, [0, 0, 0]); // different width
	const shot = compareShots({ width: 100, height: 76, data: a }, { width: 120, height: 76, data: b });
	assert.equal(shot.kind, 'size');
});

test('the diff image is one BGRA pixel per source pixel, with differences painted red', () => {
	const w = 10;
	const h = 10;
	const a = solid(w, h, [200, 200, 200]);
	const b = a.slice();
	b[(2 * w + 3) * 4 + 2] = 20; // one pixel differs in red
	const diff = makeDiffBitmap(a, b, w, h);
	assert.equal(diff.length, w * h * 4);
	const p = (2 * w + 3) * 4;
	assert.deepEqual([diff[p], diff[p + 1], diff[p + 2]], [0, 0, 255], 'the differing pixel is red');
	assert.equal(diff[p + 3], 255);
	// an unchanged pixel stays the dimmed baseline, not red
	const q = 0;
	assert.ok(diff[q + 2] < a[q + 2], 'a matching pixel is dimmed, not bright');
});
