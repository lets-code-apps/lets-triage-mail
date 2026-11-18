// The pixel comparison for the desktop screenshot baseline. Pure and dependency-free on purpose:
// it takes the raw BGRA bytes Electron's `nativeImage.toBitmap()` hands back and returns a count.
// That means plain `node --test test/shots-compare.test.ts` proves the whole diff algorithm with
// no GUI, no Electron and no image files, exactly like `popup-model.ts` proves the popup's text
// without a page. `render.ts` is the only place that decodes a PNG into these bytes.
//
// `nativeImage` pixels are BGRA, premultiplied, 4 bytes per pixel. We treat a pixel as "different"
// only when its biggest single-channel gap is over PIXEL_DELTA; a shot "changes" when the fraction
// of different pixels is over MAX_CHANGED_RATIO. The delta absorbs a runner image's antialiasing
// jitter; the ratio is the real budget (a genuine layout or data change moves far more than 0.1%).

export const PIXEL_DELTA = 16;
export const MAX_CHANGED_RATIO = 0.001; // 0.1%: about 840 px at 1100x760, 274 px at 360x760.

// A decoded image: its CSS-pixel size plus the BGRA buffer, exactly `width * height * 4` bytes long.
export type Bitmap = { width: number; height: number; data: Uint8Array };

// Count pixels that differ, given both buffers are the same `w * h * 4` size. This is the inner
// loop of `compareShots`; it is exported so a test can feed it two hand-built buffers directly.
export function diffBitmaps(a: Uint8Array, b: Uint8Array, w: number, h: number, delta: number = PIXEL_DELTA): { changed: number; ratio: number } {
	const total = w * h;
	let changed = 0;
	for (let i = 0; i < total; i += 1) {
		const p = i * 4;
		const db = Math.abs(a[p] - b[p]);
		const dg = Math.abs(a[p + 1] - b[p + 1]);
		const dr = Math.abs(a[p + 2] - b[p + 2]);
		if (Math.max(db, dg, dr) > delta) changed += 1;
	}
	return { changed, ratio: total === 0 ? 0 : changed / total };
}

// Decide one shot. A size difference is its own verdict ("SIZE"), so a reflow that changes the
// window size is reported as such rather than crashing the byte loop on mismatched lengths.
export function compareShots(actual: Bitmap, baseline: Bitmap, opts: { delta?: number; maxRatio?: number } = {}): { kind: 'ok' | 'changed' | 'size'; changed: number; ratio: number } {
	const delta = opts.delta ?? PIXEL_DELTA;
	const maxRatio = opts.maxRatio ?? MAX_CHANGED_RATIO;
	if (actual.width !== baseline.width || actual.height !== baseline.height) {
		return { kind: 'size', changed: 0, ratio: 0 };
	}
	const { changed, ratio } = diffBitmaps(actual.data, baseline.data, actual.width, actual.height, delta);
	return { kind: ratio > maxRatio ? 'changed' : 'ok', changed, ratio };
}

// A human-readable diff image for the CI artifact: the baseline dimmed to a quarter, with every
// differing pixel painted solid red (BGRA 0,0,255). `render.ts` encodes this back to PNG for any
// shot that came back `changed`, so a reviewer sees WHERE a shot moved, not just that it did.
export function makeDiffBitmap(baseline: Uint8Array, actual: Uint8Array, w: number, h: number, delta: number = PIXEL_DELTA): Uint8Array {
	const total = w * h;
	const out = new Uint8Array(total * 4);
	for (let i = 0; i < total; i += 1) {
		const p = i * 4;
		const db = Math.abs(baseline[p] - actual[p]);
		const dg = Math.abs(baseline[p + 1] - actual[p + 1]);
		const dr = Math.abs(baseline[p + 2] - actual[p + 2]);
		if (Math.max(db, dg, dr) > delta) {
			out[p] = 0;
			out[p + 1] = 0;
			out[p + 2] = 255;
		} else {
			out[p] = baseline[p] >> 2;
			out[p + 1] = baseline[p + 1] >> 2;
			out[p + 2] = baseline[p + 2] >> 2;
		}
		out[p + 3] = 255;
	}
	return out;
}
