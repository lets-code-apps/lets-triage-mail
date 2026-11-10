// The menu-bar glyph on a fixed canvas, with or without macOS's "item is open" highlight pill.
// Pure pixel code, free of `electron`, so `node --test` can check it. macOS highlights a menu-bar
// item while its menu is open, but our popup is a window, so main swaps in the pill version instead.
//
// Sizes are in physical pixels of the @2x image: the native pill is about 22 pt tall and a few pt
// wider than the glyph on each side, so the canvas is 28 x 22 pt = 56 x 44 px around the 18 pt glyph.
// Both versions use the same canvas, glyph centred, so the menu bar never shifts when they swap.

export const CANVAS = { width: 56, height: 44 };
const RADIUS = 12; // 6 pt, in px
const PILL_ALPHA = 64; // ~25%, the design's "OS highlight pill rgba(255,255,255,.25)"

// `glyph` is a BGRA bitmap (Electron's `toBitmap()`); the result is BGRA too. The template image
// only uses alpha, so every output pixel is black and just the alpha channel varies.
export function trayBitmap(glyph: Uint8Array, glyphWidth: number, glyphHeight: number, pill: boolean): Buffer {
	const { width, height } = CANVAS;
	const out = Buffer.alloc(width * height * 4); // all zero: transparent black
	const left = Math.round((width - glyphWidth) / 2);
	const top = Math.round((height - glyphHeight) / 2);
	for (let y = 0; y < height; y += 1) {
		for (let x = 0; x < width; x += 1) {
			let alpha = 0;
			if (pill) {
				// Distance outside the rounded rectangle's inner box; a 1 px ramp antialiases the edge.
				const dx = Math.max(RADIUS - (x + 0.5), x + 0.5 - (width - RADIUS), 0);
				const dy = Math.max(RADIUS - (y + 0.5), y + 0.5 - (height - RADIUS), 0);
				alpha = PILL_ALPHA * Math.min(1, Math.max(0, RADIUS - Math.hypot(dx, dy) + 0.5));
			}
			const gx = x - left;
			const gy = y - top;
			if (gx >= 0 && gy >= 0 && gx < glyphWidth && gy < glyphHeight) {
				const glyphAlpha = glyph[(gy * glyphWidth + gx) * 4 + 3];
				alpha = glyphAlpha + alpha * (1 - glyphAlpha / 255); // glyph over pill
			}
			out[(y * width + x) * 4 + 3] = Math.round(alpha);
		}
	}
	return out;
}
