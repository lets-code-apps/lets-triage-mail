// The design layout assertions for the desktop window: the app-bar title centred with 132px of
// clear, the emblem pinned to the right end, the settings rows aligned, and the controls meeting
// the design's 44px touch-target. The *decision* is a pure function, so plain `node --test`
// (test/shots-layout.test.ts) proves the rules on hand-written numbers with no GUI. `render.ts`
// runs `MEASURE` in the live page and feeds the same function the eight real states.
//
// This file has no Electron and no `node:` imports beyond types, so it is safe to import from
// both the unit test and the Electron entry. It also carries no personal data: like the shots
// themselves, every number here is either a design constant or a synthetic measurement.

// The four controls the app does not yet size to 44, with their measured heights as a FLOOR. This is
// a ratchet, not an exemption: they may grow (a later design change raises the floor), but CI turns
// red the moment one shrinks. Every other visible button, and every `.btn` width, must be >= 44.
// Raising these to 44 is a separate design change (out of scope here), which is why the floor is
// the current size rather than 44. The nav floors are set a hair under the 41.7px the nav buttons
// measure, so a fractional change is tolerated but a shrink is caught.
export const KNOWN_BELOW_44 = { nav: 41, seg: 38, switch: 24 } as const;
export type ControlRole = 'btn' | 'nav' | 'seg' | 'switch' | 'other';

const CONTROL_LABEL: Record<ControlRole, string> = {
	btn: '.btn',
	nav: '.nav button',
	seg: '.seg button',
	switch: 'input.switch',
	other: 'control',
};

const r2 = (n: number): number => Math.round(n * 100) / 100;

export type Rect = { left: number; right: number; width: number; height: number; centerX: number; centerY: number };
export type Row = { left: number; right: number; top: number; bottom: number };
export type Control = { role: ControlRole; width: number; height: number };
export type Title = Rect & { scrollWidth: number; clientWidth: number };

export type Measurements = {
	width: number; // window CSS width (1100 or 360)
	title: Title;
	emblem: Rect;
	bar: { right: number; centerY: number };
	srows: Row[];
	controls: Control[];
	switchRowHeight: number; // the row wrapping the visible switch: `.srow` on the settings screen, `.switch-row` in the setup column (the digest step), -1 when no switch is shown
};

// One title-centring budget, spelled once: 132px of clear on each side (the design's 264px of slack
// around the title), and the 24px inset (tokens.css --s-6) from the bar's right that the emblem uses.
const TITLE_CLEAR = 132;
const EMBLEM_INSET = 24;

// Returns the list of violations; empty means every rule holds. Each message names the rule it
// broke, so a red job points at the exact assertion (the plan's test cases assert on these names).
export function checkLayout(state: 'accounts' | 'settings', m: Measurements): string[] {
	const v: string[] = [];

	// 1. The app name is centred on the whole bar, keeps 132px clear of the traffic lights and the
	// emblem, stays on one line, and does not collide with the emblem or overflow its own box.
	if (Math.abs(m.title.centerX - m.width / 2) > 1) {
		v.push(`title not centred: centerX ${r2(m.title.centerX)} vs W/2 ${m.width / 2}`);
	}
	if (m.title.left < TITLE_CLEAR - 0.5) {
		v.push(`title not centred: left ${r2(m.title.left)} < ${TITLE_CLEAR}px clear`);
	}
	if (m.title.right > m.width - TITLE_CLEAR + 0.5) {
		v.push(`title not centred: right ${r2(m.title.right)} > W - ${TITLE_CLEAR}px clear`);
	}
	if (m.title.height > 24) {
		v.push(`title not one line: height ${r2(m.title.height)} > 24`);
	}
	if (m.title.right > m.emblem.left) {
		v.push(`title intersects emblem: title.right ${r2(m.title.right)} > emblem.left ${r2(m.emblem.left)}`);
	}
	if (m.title.scrollWidth > m.title.clientWidth) {
		v.push(`title overflows its box: scrollWidth ${m.title.scrollWidth} > clientWidth ${m.title.clientWidth}`);
	}

	// 2. The emblem sits at the right end of the bar (bar.right minus the 24px inset), vertically
	// centred, clear of the title.
	if (Math.abs(m.emblem.right - (m.bar.right - EMBLEM_INSET)) > 1) {
		v.push(`emblem not at the right end: right ${r2(m.emblem.right)} vs bar.right ${r2(m.bar.right)} - ${EMBLEM_INSET}`);
	}
	if (Math.abs(m.emblem.centerY - m.bar.centerY) > 1) {
		v.push(`emblem not vertically centred: centerY ${r2(m.emblem.centerY)} vs bar.centerY ${r2(m.bar.centerY)}`);
	}
	if (m.emblem.left < m.title.right) {
		v.push(`emblem overlaps title: emblem.left ${r2(m.emblem.left)} < title.right ${r2(m.title.right)}`);
	}

	// 3. The settings rows line up on a shared column and stack without overlap (settings only).
	if (state === 'settings') {
		if (m.srows.length < 5) {
			v.push(`settings rows not aligned: only ${m.srows.length} row(s), expected at least 5`);
		}
		const first = m.srows[0];
		if (first) {
			for (const row of m.srows) {
				if (Math.abs(row.left - first.left) > 0.5) {
					v.push(`settings rows not aligned: row left ${r2(row.left)} vs first ${r2(first.left)}`);
					break;
				}
				if (Math.abs(row.right - first.right) > 0.5) {
					v.push(`settings rows not aligned: row right ${r2(row.right)} vs first ${r2(first.right)}`);
					break;
				}
			}
			for (let i = 1; i < m.srows.length; i += 1) {
				if (m.srows[i].top < m.srows[i - 1].bottom - 0.5) {
					v.push(`settings rows not aligned: row ${i} top ${r2(m.srows[i].top)} above previous bottom ${r2(m.srows[i - 1].bottom)}`);
					break;
				}
			}
		}
	}

	// 4. Touch targets: >= 44px unless the control is in KNOWN_BELOW_44, where the measured height
	// is the floor it may never fall below. A `.btn` must also be >= 44 wide. The login switch is 24
	// tall by design, so its wrapping row must be >= 44 tall instead.
	for (const c of m.controls) {
		const floor = c.role in KNOWN_BELOW_44 ? KNOWN_BELOW_44[c.role as keyof typeof KNOWN_BELOW_44] : 44;
		if (c.height < floor) {
			v.push(`${CONTROL_LABEL[c.role]} height ${r2(c.height)} below floor ${floor}`);
		}
		if (c.role === 'btn' && c.width < 44) {
			v.push(`.btn width ${r2(c.width)} below 44`);
		}
	}
	if (m.switchRowHeight >= 0 && m.switchRowHeight < 44) {
		v.push(`switch row height ${r2(m.switchRowHeight)} below 44`);
	}

	return v;
}

// The in-page measuring snippet, as a string (so `render.ts` can pass it to
// `webContents.executeJavaScript` and the unit test can assert on its text without running it).
// It reads only getBoundingClientRect and the switch's row; it changes nothing and reads no data
// from the engine. The role order matters: the switch (an input) first, then `.btn`, then the radio
// segment, then the nav buttons, everything else "other".
export const MEASURE = `
(() => {
  const box = (el) => {
    const r = el.getBoundingClientRect();
    return { left: r.left, right: r.right, width: r.width, height: r.height, centerX: r.left + r.width / 2, centerY: r.top + r.height / 2 };
  };
  const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const role = (el) =>
    el.classList.contains('switch') ? 'switch' :
    el.classList.contains('btn') ? 'btn' :
    el.closest('.seg') ? 'seg' :
    el.closest('.nav') ? 'nav' : 'other';
  const titleEl = document.querySelector('.appbar-title');
  const emblemEl = document.querySelector('.emblem');
  const barEl = document.querySelector('.appbar');
  const title = box(titleEl);
  title.scrollWidth = titleEl.scrollWidth;
  title.clientWidth = titleEl.clientWidth;
  const switchEl = document.querySelector('.switch');
  const switchShown = !!switchEl && switchEl.getBoundingClientRect().height > 0;
  const rowEl = switchShown ? switchEl.closest('.srow') ?? switchEl.closest('.switch-row') : null;
  return {
    width: window.innerWidth,
    title,
    emblem: box(emblemEl),
    bar: { right: barEl.getBoundingClientRect().right, centerY: barEl.getBoundingClientRect().top + barEl.getBoundingClientRect().height / 2 },
    srows: [...document.querySelectorAll('.srow')].filter(visible).map((el) => { const r = el.getBoundingClientRect(); return { left: r.left, right: r.right, top: r.top, bottom: r.bottom }; }),
    controls: [...document.querySelectorAll('button, input')].filter(visible).map((el) => { const r = el.getBoundingClientRect(); return { role: role(el), width: r.width, height: r.height }; }),
    switchRowHeight: rowEl ? rowEl.getBoundingClientRect().height : -1,
  };
})()
`;

// --- the popup counts strip -----------------------------------------------------------
// The window measures the app bar and settings rows; the popup measures its counts strip. The rules
// are the design's: three equal cells that meet the 44px touch target, each a real button reachable by
// keyboard, each named with its count (so a screen reader hears "needs reply, 4, open Apple Mail"), and
// the decorative dot and corner arrow hidden from assistive tech. `render.ts` runs MEASURE_POPUP in the
// live page and feeds the same pure function the six real popup states.
export type PopupCell = {
	tag: string; // tagName of the cell element
	width: number;
	height: number;
	ariaLabel: string; // the whole cell's accessible name
	dotHidden: boolean; // the dot, when the cell has one, is aria-hidden (true when there is no dot)
	iconHidden: boolean; // the corner arrow is aria-hidden
	num?: Box | null; // boxes of the parts, null when absent or hidden
	dot?: Box | null;
	icon?: Box | null;
	lbl?: Box | null;
};

export type PopupMeasure = {
	cells: PopupCell[];
	noteFilled: boolean; // the strip-note line has text (normal/calm/paused all fill it)
};

// Returns the list of violations; empty means every rule holds. Messages name the rule, so a red job
// points at the exact assertion.
export function checkPopupLayout(m: PopupMeasure): string[] {
	const v: string[] = [];

	if (m.cells.length !== 3) {
		v.push(`popup strip must have 3 cells, saw ${m.cells.length}`);
		return v; // nothing else is meaningful with the wrong cell count
	}

	const firstWidth = m.cells[0].width;
	for (let i = 0; i < m.cells.length; i += 1) {
		const c = m.cells[i];
		if (c.tag !== 'BUTTON') {
			v.push(`cell ${i} is <${c.tag.toLowerCase()}>, expected a <button> for keyboard reach`);
		}
		if (c.height < 44) {
			v.push(`cell ${i} height ${r2(c.height)} below the 44px touch target`);
		}
		if (!/, \d+, open Apple Mail$/.test(c.ariaLabel)) {
			v.push(`cell ${i} aria-label must name the count and the action, saw "${c.ariaLabel}"`);
		}
		if (!c.dotHidden) {
			v.push(`cell ${i} dot must be aria-hidden`);
		}
		if (!c.iconHidden) {
			v.push(`cell ${i} icon must be aria-hidden`);
		}
		// CountStrip: number, dot and arrow share the top line (arrow at the right edge); label below.
		const mid = (b: Box) => b.top + b.height / 2;
		if (c.num && c.icon && Math.abs(mid(c.icon) - mid(c.num)) > 4) {
			v.push(`cell ${i} arrow is not on the number's line`);
		}
		if (c.num && c.dot && Math.abs(mid(c.dot) - mid(c.num)) > 4) {
			v.push(`cell ${i} dot is not on the number's line`);
		}
		if (c.num && c.dot && c.dot.left < c.num.left + c.num.width) {
			v.push(`cell ${i} dot must come after the number`);
		}
		if (c.num && c.lbl && c.lbl.top < c.num.top + c.num.height - 1) {
			v.push(`cell ${i} label must sit below the number`);
		}
	}

	// The three cells share the strip equally: equal widths (within 1px of a rounding gap).
	for (const c of m.cells) {
		if (Math.abs(c.width - firstWidth) > 1) {
			v.push(`cells not equal width: ${r2(c.width)} vs first ${r2(firstWidth)}`);
			break;
		}
	}

	if (!m.noteFilled) {
		v.push('the strip note line is empty');
	}

	return v;
}

// The in-page measuring snippet for the popup (a string, so `render.ts` can pass it to executeJavaScript
// and the unit test can assert on its text without running it). Reads only getBoundingClientRect and the
// aria/hidden attributes; changes nothing and reads no engine data.
type Box = { left: number; top: number; width: number; height: number };

export const MEASURE_POPUP = `
(() => {
  const visible = (el) => { const r = el.getBoundingClientRect(); return r.width > 0 && r.height > 0; };
  const cells = [...document.querySelectorAll('#strip .cell')].filter(visible).map((el) => {
    const r = el.getBoundingClientRect();
    const dot = el.querySelector('.dot');
    const icon = el.querySelector('svg.icon');
    const box = (n) => { if (!n || !visible(n)) return null; const b = n.getBoundingClientRect(); return { left: b.left, top: b.top, width: b.width, height: b.height }; };
    return {
      num: box(el.querySelector('.num')),
      dot: box(dot),
      icon: box(icon),
      lbl: box(el.querySelector('.lbl')),
      tag: el.tagName,
      width: r.width,
      height: r.height,
      ariaLabel: el.getAttribute('aria-label') ?? '',
      dotHidden: dot ? dot.getAttribute('aria-hidden') === 'true' : true,
      iconHidden: icon ? icon.getAttribute('aria-hidden') === 'true' : true,
    };
  });
  const note = document.querySelector('#note');
  return { cells, noteFilled: !!note && note.textContent.trim() !== '' };
})()
`;
