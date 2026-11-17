// Window policy: closing the window hides it; only an explicit Quit ends the app. This holds
// the small state (has the window been created, are we quitting?) so it can be tested without a
// real Electron BrowserWindow or the macOS dock. The main process supplies the real window and
// dock behind these tiny interfaces.
//
// Contract (docs/DESKTOP.md):
//   - close (red button, Cmd+W) is prevented and hides the window; the dock hides when nothing
//     is visible.
//   - reopening reuses the SAME window (created once, never destroyed).
//   - while quitting, close is NOT prevented (the window must actually go away).
//   - `window-all-closed` does nothing: the app keeps running with no window.

export interface WindowHandle {
	isVisible(): boolean;
	hide(): void;
	show(): void;
}

export interface Dock {
	hide(): void;
	show(): void;
}

// The close event carries preventDefault(); the fake used in tests records whether it was called.
export interface CloseEvent {
	preventDefault(): void;
}

export class WindowController {
	private handle: WindowHandle | null = null;
	private quitting = false;
	private readonly createWindow: () => WindowHandle;
	private readonly dock: Dock;

	constructor(createWindow: () => WindowHandle, dock: Dock) {
		this.createWindow = createWindow;
		this.dock = dock;
	}

	// Create on first use, then always show/show the dock. Never destroys a window.
	open(): void {
		if (this.handle === null) this.handle = this.createWindow();
		this.handle.show();
		this.dock.show();
	}

	// Called from `before-quit` once the user has chosen to quit.
	beginQuitting(): void {
		this.quitting = true;
	}

	// The window `close` handler.
	handleClose(event: CloseEvent): void {
		// A real close is fine while quitting; otherwise hide instead of destroy.
		if (this.quitting) return;
		event.preventDefault();
		this.handle?.hide();
		if (this.handle !== null && !this.handle.isVisible()) this.dock.hide();
	}

	// Bound to `window-all-closed`. Deliberately empty: on macOS the app stays alive with no window.
	handleWindowAllClosed(): void {
		// Intentionally does nothing.
	}
}
