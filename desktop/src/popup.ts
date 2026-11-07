// The popup window's behaviour, free of any `electron` import so plain `node --test` can drive it
// (the same trick `window.ts` uses). The main process supplies a real `BrowserWindow` behind
// `PopupHandle` (docs/DESKTOP.md: the popup is frameless, never resized, and never touches the dock).
//
// Why a grace period: clicking the menu-bar icon while the popup is open fires two events, in this
// order — the window loses focus (we hide), then the tray click arrives (which would open it again).
// Without the pause the popup flickers closed and open on one click. macOS gives no "this is the
// same gesture" flag, so timing is the only signal: a click within ~250 ms of our own hide is
// treated as that same click, not a new one.

// How long a hidden popup ignores the tray click that follows its own blur-hide.
export const HIDE_GRACE_MS = 250;

// The popup's fixed size (360 px wide, per the design). Main passes it to `popupPosition`.
export const POPUP_SIZE = { width: 360, height: 260 } as const;

// What the main process can do to the popup window. Kept to three methods so a test can watch
// exactly what happens, and so nothing here can move or resize a real window.
export interface PopupHandle {
	isVisible(): boolean;
	show(): void;
	hide(): void;
}

// A rectangle in screen coordinates: the tray icon's bounds and the work area (the screen minus
// the menu bar and any dock). `BrowserWindow#getBounds()` and `screen.getWorkAreaBounds()` both
// hand back exactly this shape.
export interface Bounds {
	x: number;
	y: number;
	width: number;
	height: number;
}

/**
 * Where to put the popup, centred under the tray icon.
 *
 * Pure and unit-tested because the two clamps are the whole craft of it: a 360 px window under an
 * 18 px icon near a screen edge must stay fully inside the work area (macOS would otherwise refuse
 * or shift it unpredictably), and it always sits below the menu bar.
 */
export function popupPosition(
	tray: Bounds,
	workArea: Bounds,
	size: { width: number; height: number } = POPUP_SIZE,
): { x: number; y: number } {
	const centre = tray.x + tray.width / 2;
	const rawX = centre - size.width / 2;
	// Clamp horizontally against the work area, never past the right edge.
	const x = Math.max(workArea.x, Math.min(rawX, workArea.x + workArea.width - size.width));
	// A 4 px gap keeps the popup off the menu-bar separator line.
	const y = Math.min(tray.y + tray.height + 4, workArea.y + workArea.height - size.height);
	return { x: Math.round(x), y: Math.round(y) };
}

export class PopupController {
	private handle: PopupHandle | null = null;
	// When we hid the popup ourselves; `trayClick` compares against this.
	private lastHideAt = -Infinity;
	private readonly createWindow: () => PopupHandle;
	// Which clock to use is injected: main passes `Date.now`, a test moves time at will.
	private readonly clock: () => number;

	// The window is created on first use and only ever hidden afterwards.
	constructor(createWindow: () => PopupHandle, clock: () => number = Date.now) {
		this.createWindow = createWindow;
		this.clock = clock;
	}

	// Open on first use, then show/toggle. Never destroys a window: hiding is what closes it.
	private ensure(): PopupHandle {
		if (this.handle === null) this.handle = this.createWindow();
		return this.handle;
	}

	isVisible(): boolean {
		return this.handle !== null && this.handle.isVisible();
	}

	// The tray icon's left click: one gesture toggles the popup.
	toggle(): void {
		const window = this.ensure();
		if (window.isVisible()) window.hide();
		else window.show();
	}

	// The popup's `blur` event: clicking anywhere else closes it, like a native menu.
	handleBlur(): void {
		if (this.handle === null || !this.handle.isVisible()) return;
		this.handle.hide();
		this.lastHideAt = this.clock();
	}

	// The tray icon's click, given the clock so the grace period is testable.
	trayClick(at: number): void {
		if (at - this.lastHideAt < HIDE_GRACE_MS) return; // the click that just blurred us
		this.toggle();
	}
}
