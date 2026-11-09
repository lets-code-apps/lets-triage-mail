// The popup window's behaviour, free of any `electron` import so plain `node --test` can drive it
// (the same trick `window.ts` uses). The main process supplies a real `BrowserWindow` behind
// `PopupHandle` (docs/DESKTOP.md: the popup is frameless, and it is resized and placed by this module
// only — the user cannot drag or resize it — and it never touches the dock).
//
// Why a grace period: clicking the menu-bar icon while the popup is open fires two events, in this
// order — the window loses focus (we hide), then the tray click arrives (which would open it again).
// Without the pause the popup flickers closed and open on one click. macOS gives no "this is the
// same gesture" flag, so timing is the only signal: a click within ~250 ms of our own hide is
// treated as that same click, not a new one.

// How long a hidden popup ignores the tray click that follows its own blur-hide.
export const HIDE_GRACE_MS = 250;

// The popup's design width (360 px). The height is *measured*, not designed: main reads the page's
// content height after every status paint and caches it here; until the first measurement lands,
// the fallback below is the height (360x260 was the old fixed size, which suits a short status).
export const POPUP_WIDTH = 360;
export const POPUP_FALLBACK_HEIGHT = 260;

// What the main process can do to the popup window. Kept to five methods so a test can watch
// exactly what happens. `setBounds` is the one resize/move path: this module alone calls it, and
// it always places the window fully inside the display's work area.
export interface PopupHandle {
	isVisible(): boolean;
	show(): void;
	hide(): void;
	setBounds(bounds: Bounds): void;
	// macOS virtual desktops ("Spaces"): several desktops can share one monitor, and a window that
	// belongs to a desktop you are not looking at pulls you back to it when it is shown. So the popup
	// claims the desktop it is being shown on *before* `show`, and never releases the claim: an
	// Electron call that reverses it flips the app's process type, which briefly hides the window
	// and the Dock, and a hidden window draws nothing on any desktop anyway. The main process applies
	// it at most once per window for the same reason.
	followCurrentSpace(): void;
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

// Where the popup should hang: the tray icon's rectangle and the work area of the display *that
// holds the icon*. The main process looks both up fresh on every open (macOS with "separate
// Spaces" runs one menu bar per display, so the icon's rectangle tells us which one was clicked).
export interface PopupAnchor {
	tray: Bounds;
	workArea: Bounds;
}

/**
 * The popup's size for a measured content height, capped to the display's work area.
 *
 * Pure and unit-tested because the rules are easy to get wrong: round the content up (a fractional
 * px would bring back a scrollbar), never trust the measurement (0/NaN/Infinity means "keep the
 * fallback"), and cap both dimensions so a tall status on a small display still fits on it.
 */
export function fitSize(
	contentHeight: number,
	workArea: Bounds,
): { width: number; height: number } {
	const width = Math.min(POPUP_WIDTH, workArea.width);
	// A non-finite or non-positive height means the measurement failed: keep the fallback height
	// rather than shrink the window to nothing (a failed measure must never clip the popup).
	const raw = Number.isFinite(contentHeight) && contentHeight > 0 ? Math.ceil(contentHeight) : POPUP_FALLBACK_HEIGHT;
	const height = Math.min(raw, workArea.height);
	return { width, height };
}

/**
 * Where to put the popup, centred under the tray icon.
 *
 * Pure and unit-tested because the three clamps are the whole craft of it: a 360 px window under an
 * 18 px icon near a screen edge must stay fully inside the work area (macOS would otherwise refuse
 * or shift it unpredictably), it always sits below the menu bar, and — since the work area of a
 * display left of or above the primary has negative x/y — every clamp is against *that display's*
 * work area, never against 0.
 */
export function popupPosition(
	tray: Bounds,
	workArea: Bounds,
	size: { width: number; height: number },
): { x: number; y: number } {
	const centre = tray.x + tray.width / 2;
	const rawX = centre - size.width / 2;
	// Clamp horizontally against the work area, never past either edge.
	const x = Math.max(workArea.x, Math.min(rawX, workArea.x + workArea.width - size.width));
	// A 4 px gap keeps the popup off the menu-bar separator line; the lower clamp keeps it on the
	// display even when the capped height equals the work area; the upper clamp stops an icon above
	// the top edge (secondary displays have negative y) from pulling the popup off-screen.
	const y = Math.max(workArea.y, Math.min(tray.y + tray.height + 4, workArea.y + workArea.height - size.height));
	return { x: Math.round(x), y: Math.round(y) };
}

export class PopupController {
	private handle: PopupHandle | null = null;
	// When we hid the popup ourselves; `trayClick` compares against this.
	private lastHideAt = -Infinity;
	private readonly createWindow: () => PopupHandle;
	// Where the icon and its display are *now*: read on every open, because the popup must follow
	// the display the user clicked, not the one the window was first created on.
	private readonly anchor: () => PopupAnchor;
	// Which clock to use is injected: main passes `Date.now`, a test moves time at will.
	// (Required-after-optional parameters are illegal, so `anchor` sits before the optional `clock`.)
	private readonly clock: () => number;
	// The last measured content height, and the last bounds we applied. The cache matters because
	// showing is synchronous but measuring is async: an open uses the cached height, and a visible
	// popup is corrected after each poll.
	private contentHeight: number | null = null;
	private lastBounds: Bounds | null = null;

	// The window is created on first use and only ever hidden afterwards.
	constructor(createWindow: () => PopupHandle, anchor: () => PopupAnchor, clock: () => number = Date.now) {
		this.createWindow = createWindow;
		this.anchor = anchor;
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

	// Size and place the window for the current display and cached height. Only applied when the
	// bounds actually change, so the popup never flickers between identical polls.
	private place(window: PopupHandle): void {
		const { tray, workArea } = this.anchor();
		const size = fitSize(this.contentHeight ?? POPUP_FALLBACK_HEIGHT, workArea);
		const at = popupPosition(tray, workArea, size);
		const bounds: Bounds = { x: at.x, y: at.y, width: size.width, height: size.height };
		const same =
			this.lastBounds !== null &&
			this.lastBounds.x === bounds.x &&
			this.lastBounds.y === bounds.y &&
			this.lastBounds.width === bounds.width &&
			this.lastBounds.height === bounds.height;
		if (same) return;
		window.setBounds(bounds);
		this.lastBounds = bounds;
	}

	// The tray icon's left click: one gesture toggles the popup. Placement happens just before
	// showing so the popup appears under the clicked icon on the clicked display, already sized. The
	// Space claim comes first of all: it has to be in place before the window appears, or macOS
	// switches the user back to the desktop the window last opened on.
	toggle(): void {
		const window = this.ensure();
		if (window.isVisible()) window.hide();
		else {
			window.followCurrentSpace();
			this.place(window);
			window.show();
		}
	}

	// A fresh content height from main's measurement of the page. While visible, resize immediately
	// (a poll just changed the rows); while hidden, just cache — the next open uses the cached value.
	setContentHeight(height: number): void {
		this.contentHeight = height;
		const window = this.handle;
		if (window !== null && window.isVisible()) this.place(window);
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
