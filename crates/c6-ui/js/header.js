// Header fitting is event-driven, not a render/scroll/animation-frame loop.
// Overflowed items keep their intrinsic size offscreen (and are inert), so the
// measurement inputs do not change when its outputs do. Never reparent Leptos
// nodes: the drawer already contains the same live links and their badges.
window.c6HeaderFit = header => {
  const items = [...header.querySelectorAll('[data-header-item]')];
  const fixed = [...header.querySelectorAll('[data-header-fixed]')];
  const menu = header.querySelector('[data-header-menu]');
  const button = menu.querySelector('button');
  const note = menu.querySelector('[data-header-overflow-note]');
  const ordered = [...items].sort((a, b) => Number(a.dataset.headerPriority) - Number(b.dataset.headerPriority));
  let disposed = false;
  const attribute = (el, name, value) => {
    if (value === null) { if (el.hasAttribute(name)) el.removeAttribute(name); }
    else if (el.getAttribute(name) !== value) el.setAttribute(name, value);
  };
  const fit = () => {
    if (disposed || !header.isConnected || !header.clientWidth) return;
    // All geometry reads precede all writes, once per changed layout. The
    // offscreen items still have exactly the width they would have in the bar.
    const css = getComputedStyle(header);
    const gap = parseFloat(css.columnGap) || 0;
    const available = header.clientWidth - parseFloat(css.paddingLeft) - parseFloat(css.paddingRight);
    const widths = new Map(items.map(el => [el, el.getBoundingClientRect().width]));
    const active = items.filter(el => widths.get(el) > 0);
    // The zero-minimum flex spacer is also a child and consumes one gap.
    const base = fixed.reduce((sum, el) => sum + el.getBoundingClientRect().width, 0) + gap * fixed.length;
    let required = base + active.reduce((sum, el) => sum + widths.get(el) + gap, 0);
    const moved = new Set();
    const open = button.getAttribute('aria-expanded') === 'true';
    if (required > available || open) {
      required += menu.getBoundingClientRect().width + gap;
      for (const el of ordered) {
        if (required <= available) break;
        if (!widths.get(el)) continue;
        moved.add(el);
        required -= widths.get(el) + gap;
      }
    }
    let count = 0;
    const focused = document.activeElement;
    const focusMoved = items.some(el => moved.has(el) && el.contains(focused));
    for (const el of items) {
      const overflow = moved.has(el);
      const hidden = overflow || !widths.get(el);
      attribute(el, 'data-header-overflow', overflow ? '' : null);
      attribute(el, 'data-header-empty', !widths.get(el) ? '' : null);
      attribute(el, 'inert', hidden ? '' : null);
      attribute(el, 'aria-hidden', hidden ? 'true' : null);
      if (overflow) for (const value of el.querySelectorAll('[data-header-count]')) {
        count += Number(value.textContent) || 0;
      }
    }
    // Don't conceal an open drawer (or leave it invisibly open on widening).
    const showMenu = moved.size || open;
    attribute(menu, 'data-header-menu-hidden', showMenu ? null : '');
    attribute(menu, 'inert', showMenu ? null : '');
    attribute(menu, 'aria-hidden', showMenu ? null : 'true');
    attribute(menu, 'data-header-counted', count ? 'true' : null);
    const description = count ? `${count} waiting on navigation items in this menu` : 'More navigation';
    if (note.textContent !== description) note.textContent = description;
    attribute(button, 'title', description);
    attribute(header, 'data-header-ready', '');
    if (focusMoved) button.focus({ preventScroll: true });
    // Escape can close a drawer widened until every item fits. Its opener
    // then goes away too; leave focus on live navigation, not an inert button.
    if (!showMenu && menu.contains(focused)) {
      const search = items.find(el => el.dataset.headerItem === 'search' && !el.hasAttribute('inert'));
      (search?.querySelector('input') || header.querySelector('[data-header-fixed] a'))?.focus({ preventScroll: true });
    }
  };
  const sizes = new ResizeObserver(fit);
  [header, menu, ...fixed, ...items].forEach(el => sizes.observe(el));
  // A badge can change while keeping the same width. Ignore our own feedback
  // spans; the observer deliberately doesn't watch the attributes fit writes.
  const content = new MutationObserver(records => {
    if (records.some(r => !(r.target instanceof Element ? r.target : r.target.parentElement)?.closest('[data-header-feedback]'))) fit();
  });
  content.observe(header, { childList: true, characterData: true, subtree: true });
  const opening = new MutationObserver(fit);
  opening.observe(button, { attributes: true, attributeFilter: ['aria-expanded'] });
  fit();
  document.fonts?.ready.then(fit);
  return () => { disposed = true; sizes.disconnect(); content.disconnect(); opening.disconnect(); };
};
