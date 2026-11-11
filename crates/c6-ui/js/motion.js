// Like the theme, motion belongs to this browser, never to a realm. Resolve
// System before the first paint; explicit Full deliberately ignores the OS.
window.c6Motion = (() => {
  const KEY = "c6.motion";
  const media = matchMedia("(prefers-reduced-motion: reduce)");
  const get = () => {
    let value;
    try { value = localStorage.getItem(KEY); } catch (_) {}
    return ["full", "reduced", "off"].includes(value) ? value : "system";
  };
  const apply = () => {
    const choice = get();
    const resolved = choice === "system"
      ? (media.matches ? "reduced" : "full") : choice;
    // Explicit Full must also ignore OS changes during an in-flight move.
    // Do not interrupt anything when the effective policy has not changed.
    if (document.documentElement.dataset.motion === resolved) return;
    document.documentElement.dataset.motion = resolved;
    // An explicit change also stops any in-flight Web Animations immediately.
    if (document.documentElement.dataset.motion !== "full") {
      for (const animation of document.getAnimations()) {
        // Keyframes resolve their reduced/off variables. Preserve those:
        // cancelling a flash can leave its underlying outline permanently lit.
        // Transitions, unlike keyframes, capture their starting travel, so
        // snap an in-flight one to its real state when motion is reduced.
        const css = typeof CSSAnimation !== "undefined" && animation instanceof CSSAnimation;
        if (!css) animation.cancel();
      }
    }
    dispatchEvent(new Event("c6-motion"));
  };
  media.addEventListener("change", apply);
  addEventListener("storage", event => {
    if (event.key !== KEY && event.key !== null) return;
    apply();
    dispatchEvent(new Event("c6-motion-choice"));
  });
  apply();
  return {
    get,
    set(choice) {
      try { localStorage.setItem(KEY, choice); } catch (_) {}
      apply();
      // System and Full can resolve identically. Notify radio pickers even
      // then, without interrupting animations via the effective-policy event.
      dispatchEvent(new Event("c6-motion-choice"));
    },
    reduced: () => document.documentElement.dataset.motion !== "full",
    off: () => document.documentElement.dataset.motion === "off",
  };
})();

// Composer menus live in a portal, but belong to their tile. Prefer the space
// above the control and scroll within it; the visual viewport excludes an
// overlay keyboard. Follow layout changes, not an animation-frame polling loop.
window.c6ComposerPopover = (element, anchor) => {
  const tile = anchor.closest('[data-tile-surface]');
  let frame, disposed = false;
  const fit = () => {
    frame = undefined;
    if (disposed || !element.isConnected || !anchor.isConnected) return;
    const viewport = visualViewport;
    const area = tile?.getBoundingClientRect();
    const left = Math.max(area?.left ?? 0, viewport?.offsetLeft ?? 0) + 8;
    const top = Math.max(area?.top ?? 0, viewport?.offsetTop ?? 0) + 8;
    const right = Math.max(left, Math.min(area?.right ?? innerWidth,
      (viewport?.offsetLeft ?? 0) + (viewport?.width ?? innerWidth)) - 8);
    const bottom = Math.max(top, Math.min(area?.bottom ?? innerHeight,
      (viewport?.offsetTop ?? 0) + (viewport?.height ?? innerHeight)) - 8);
    const a = anchor.getBoundingClientRect();
    const above = Math.max(0, Math.min(a.top - 4, bottom) - top);
    const below = Math.max(0, bottom - Math.max(a.bottom + 4, top));
    const up = above >= Math.min(element.scrollHeight, 160) || above >= below;
    element.style.maxWidth = `${Math.min(right - left, 576)}px`;
    element.style.maxHeight = `${up ? above : below}px`;
    const box = element.getBoundingClientRect();
    element.style.left = `${Math.max(left, Math.min(a.left, right - box.width))}px`;
    element.style.top = `${Math.max(top, Math.min(up ? a.top - 4 - box.height : a.bottom + 4, bottom - box.height))}px`;
    element.style.transformOrigin = up ? 'bottom left' : 'top left';
    element.style.visibility = 'visible';
  };
  const schedule = () => { if (!disposed && frame === undefined) frame = requestAnimationFrame(fit); };
  const sizes = new ResizeObserver(schedule);
  [element, anchor, tile].filter(Boolean).forEach(el => sizes.observe(el));
  document.addEventListener('scroll', schedule, true);
  addEventListener('resize', schedule);
  visualViewport?.addEventListener('resize', schedule);
  visualViewport?.addEventListener('scroll', schedule);
  fit();
  return () => {
    disposed = true;
    if (frame !== undefined) cancelAnimationFrame(frame);
    sizes.disconnect();
    document.removeEventListener('scroll', schedule, true);
    removeEventListener('resize', schedule);
    visualViewport?.removeEventListener('resize', schedule);
    visualViewport?.removeEventListener('scroll', schedule);
  };
};

// Start anywhere *inside* the phone drawer, not at Android's Back edge.
// Leave native vertical scrolling and taps alone until a clear outward drag.
// Release returns control to the sheet's shared CSS tokens/presence state.
window.c6DrawerSwipe = (element, close) => {
  const phone = matchMedia("(width < 48rem), (height < 32rem)");
  const direction = element.dataset.drawerSide === "right" ? 1 : -1;
  let drag, suppressClick = false;
  const reset = () => {
    const previous = drag;
    drag = undefined;
    element.removeAttribute("data-dragging");
    element.style.removeProperty("--drawer-pull");
    if (previous && element.hasPointerCapture(previous.id)) element.releasePointerCapture(previous.id);
  };
  const down = event => {
    reset();
    suppressClick = false;
    if (event.pointerType !== "touch" || !event.isPrimary || !phone.matches || element.dataset.open !== "true") return;
    const box = element.getBoundingClientRect();
    const x = getComputedStyle(element).translate.split(" ")[0];
    const translate = (parseFloat(x) || 0) * (x.endsWith("%") ? box.width / 100 : 1);
    drag = { id: event.pointerId, x: event.clientX, y: event.clientY, width: box.width, from: translate, distance: 0, claimed: false };
  };
  const move = event => {
    if (!drag || drag.id !== event.pointerId) return;
    if (element.dataset.open !== "true" || !phone.matches) { reset(); return; }
    const dx = event.clientX - drag.x, dy = event.clientY - drag.y;
    if (!drag.claimed) {
      if (Math.max(Math.abs(dx), Math.abs(dy)) < 10) return;
      // Lock the *whole* gesture to scrolling/inward motion, even if it
      // turns outward later. A diagonal must not steal a vertical scroll.
      if (dx * direction <= 0 || dx * direction <= Math.abs(dy) * 1.5) { reset(); return; }
      drag.claimed = true;
      suppressClick = true;
      element.setPointerCapture(event.pointerId);
      element.setAttribute("data-dragging", "");
    }
    event.preventDefault();
    drag.distance = Math.max(0, dx * direction);
    element.style.setProperty("--drawer-pull", `${direction * Math.max(0, Math.min(drag.width, direction * (drag.from + dx)))}px`);
  };
  const up = event => {
    if (!drag || drag.id !== event.pointerId) return;
    const dismiss = drag.claimed && drag.distance >= drag.width * .35 && element.dataset.open === "true";
    // Capture the last finger position before removing the direct-manipulation
    // rule. Both close and snap-back then transition from that displayed frame.
    if (drag.claimed) getComputedStyle(element).translate;
    reset();
    if (dismiss) close();
  };
  const cancel = event => { if (drag?.id === event.pointerId) reset(); };
  // Touch starts with implicit capture on the tapped child. Transferring it
  // to the drawer emits a bubbling loss on that child, not a cancellation.
  const lost = event => { if (event.target === element) cancel(event); };
  const click = event => {
    if (!suppressClick || event.detail === 0) return;
    suppressClick = false;
    event.preventDefault();
    event.stopImmediatePropagation();
  };
  const escape = event => { if (event.key === "Escape") reset(); };
  const policy = () => {
    // Resolve the new --travel while direct manipulation still disables
    // transitions, so switching to Reduced cannot animate the old offset.
    if (drag?.claimed) getComputedStyle(element).translate;
    reset();
  };
  element.addEventListener("pointerdown", down);
  element.addEventListener("pointermove", move, { passive: false });
  element.addEventListener("pointerup", up);
  element.addEventListener("pointercancel", cancel);
  element.addEventListener("lostpointercapture", lost);
  element.addEventListener("click", click, true);
  addEventListener("keydown", escape);
  addEventListener("c6-motion", policy);
  phone.addEventListener("change", reset);
  return () => {
    reset();
    element.removeEventListener("pointerdown", down);
    element.removeEventListener("pointermove", move);
    element.removeEventListener("pointerup", up);
    element.removeEventListener("pointercancel", cancel);
    element.removeEventListener("lostpointercapture", lost);
    element.removeEventListener("click", click, true);
    removeEventListener("keydown", escape);
    removeEventListener("c6-motion", policy);
    phone.removeEventListener("change", reset);
  };
};

// Modal phone overlays retain their DOM for exit motion, but release native
// interaction immediately on close. Inert only sibling branches, never an
// ancestor of the dialog; remember pre-existing inert state (hidden tiles).
window.c6Modal = (element, close) => {
  const opener = document.activeElement;
  const phone = matchMedia("(width < 48rem), (height < 32rem)");
  const resized = () => { if (!phone.matches) close(); };
  phone.addEventListener("change", resized);
  // Let the browser deliver its platform close request (including Android
  // Back), without inserting fake session navigation/history entries.
  const watcher = typeof CloseWatcher === 'function' ? new CloseWatcher() : null;
  if (watcher) watcher.onclose = close;
  const hidden = [];
  for (let branch = element; branch.parentElement; branch = branch.parentElement) {
    for (const sibling of branch.parentElement.children) {
      if (sibling !== branch && sibling instanceof HTMLElement && !sibling.matches('.c6-scrim') && !sibling.inert) {
        sibling.inert = true;
        hidden.push(sibling);
      }
    }
    if (branch.parentElement === document.body) break;
  }
  element.focus({ preventScroll: true });
  const keys = event => {
    if (event.key !== "Tab") return;
    const items = [...element.querySelectorAll('button, input, select, textarea, a[href], [tabindex]')]
      .filter(el => !el.disabled && el.tabIndex >= 0 && !el.closest('[inert]') && el.getClientRects().length);
    const index = items.indexOf(document.activeElement);
    if (event.shiftKey ? index <= 0 : index < 0 || index === items.length - 1) {
      event.preventDefault();
      (event.shiftKey ? items.at(-1) : items[0])?.focus();
    }
    if (!items.length) event.preventDefault();
  };
  element.addEventListener('keydown', keys);
  return () => {
    phone.removeEventListener("change", resized);
    watcher?.destroy();
    element.removeEventListener('keydown', keys);
    for (const sibling of hidden) sibling.inert = false;
    if ((element.contains(document.activeElement) || document.activeElement === document.body) && opener?.isConnected && !opener.closest('[inert]')) {
      opener.focus({ preventScroll: true });
    }
  };
};

// Observe only a small, explicitly opted-in card, never a transcript. Read
// layout only on resize, not once per animation frame. The layout is already
// final; only its picture catches up. Cancelling starts at the displayed rect.
window.c6FlipResize = element => {
  let animation;
  let before = element.getBoundingClientRect();
  const observer = new ResizeObserver(() => {
    const previous = animation;
    const reduced = c6Motion.reduced();
    const displayed = element.getBoundingClientRect();
    const opacity = previous && reduced ? getComputedStyle(element).opacity : .5;
    previous?.cancel();
    const after = element.getBoundingClientRect();
    // ResizeObserver runs after the new layout. Its displayed rect already
    // applies the interrupted scale to that *new* height; reconstruct the
    // previous picture instead of jumping to that intermediate geometry.
    const from = previous && after.height ? before.height * displayed.height / after.height : before.height;
    if (before.height && after.height && before.height !== after.height && !c6Motion.off()) {
      const current = element.animate([
        reduced ? { opacity } : { transform: `scaleY(${from / after.height})` },
        reduced ? { opacity: 1 } : { transform: "none" },
      ], { duration: reduced ? 80 : 220, easing: "cubic-bezier(.3,0,0,1)" });
      animation = current;
      current.onfinish = current.oncancel = () => { if (animation === current) animation = undefined; };
    }
    before = after;
  });
  element.style.transformOrigin = "top";
  observer.observe(element);
  return () => { observer.disconnect(); animation?.cancel(); };
};

// A comments list knows its next keys before Leptos applies the keyed diff.
// Capture there, not with a subtree observer: comment edits, streamed tokens
// and off-screen transcript rows never pay for this. Like tiles, an outgoing
// picture is inert immediately and never delays the real layout change.
const c6Lists = new WeakMap();
window.c6FlipList = (list, keys) => {
  if (c6Motion.off()) return;
  const next = new Set(JSON.parse(keys));
  const previous = c6Lists.get(list);
  if (previous?.frame) cancelAnimationFrame(previous.frame);
  const state = { frame: 0, animations: new Map(), ghosts: [] };
  const before = new Map();
  for (const element of list.children) {
    if (!element.dataset.motionKey) continue;
    before.set(element.dataset.motionKey, { element, rect: element.getBoundingClientRect() });
  }
  // Read every displayed position before cancelling any interrupted move.
  previous?.animations.forEach(animation => animation.cancel());
  previous?.ghosts.forEach(ghost => ghost.remove());
  const animate = (element, frames, duration, done = () => {}, easing = "cubic-bezier(.3,0,0,1)") => {
    const animation = element.animate(frames, { duration, easing });
    state.animations.set(element, animation);
    animation.onfinish = animation.oncancel = () => { state.animations.delete(element); done(); };
  };
  for (const [key, { element, rect }] of before) {
    if (next.has(key) || !rect.height) continue;
    const ghost = element.cloneNode(true);
    ghost.inert = true;
    ghost.setAttribute("aria-hidden", "true");
    ghost.removeAttribute("id");
    ghost.removeAttribute("data-motion-key");
    ghost.querySelectorAll("[id]").forEach(child => child.removeAttribute("id"));
    Object.assign(ghost.style, { position: "fixed", left: `${rect.left}px`, top: `${rect.top}px`,
      width: `${rect.width}px`, height: `${rect.height}px`, margin: 0, zIndex: 50,
      pointerEvents: "none", transform: "none" });
    document.body.append(ghost);
    state.ghosts.push(ghost);
    animate(ghost, [{ opacity: 1 }, { opacity: 0 }], c6Motion.reduced() ? 80 : 150, () => {
      ghost.remove();
      state.ghosts = state.ghosts.filter(item => item !== ghost);
    }, "cubic-bezier(.4,0,1,1)");
  }
  c6Lists.set(list, state);
  state.frame = requestAnimationFrame(() => {
    state.frame = 0;
    if (!list.isConnected || c6Motion.off()) { state.ghosts.forEach(ghost => ghost.remove()); return; }
    const after = Array.from(list.children).filter(element => element.dataset.motionKey)
      .map(element => ({ element, rect: element.getBoundingClientRect() }));
    for (const { element, rect } of after) {
      const old = before.get(element.dataset.motionKey)?.rect;
      if (!old) {
        animate(element, c6Motion.reduced() ? [{ opacity: 0 }, { opacity: 1 }] : [
          { opacity: 0, transform: "translateY(6px)" }, { opacity: 1, transform: "none" },
        ], c6Motion.reduced() ? 80 : 200, () => {}, "cubic-bezier(.2,0,0,1)");
        continue;
      }
      if (Math.abs(old.top - rect.top) < .5) continue;
      animate(element, c6Motion.reduced() ? [{ opacity: .5 }, { opacity: 1 }] : [
        { transform: `translateY(${old.top - rect.top}px)` }, { transform: "none" },
      ], c6Motion.reduced() ? 80 : 160);
    }
  });
};

// Keep native details/search/keyboard semantics on engines that don't yet
// interpolate ::details-content. Only an explicit opening measures the body;
// streamed output never calls this. Its reserved output area stays 192px.
const c6ToolReveals = new WeakMap();
window.c6ToolReveal = details => {
  if (!details.open || c6Motion.off()) return;
  const body = details.querySelector(":scope > div");
  if (!body) return;
  const previous = c6ToolReveals.get(body);
  const displayed = previous ? body.getBoundingClientRect().height : 0;
  previous?.cancel();
  const reduced = c6Motion.reduced();
  const native = CSS.supports("interpolate-size", "allow-keywords") && CSS.supports("selector(details::details-content)");
  if (!reduced && native) return;
  const height = body.getBoundingClientRect().height;
  const animation = body.animate(reduced ? [{ opacity: 0 }, { opacity: 1 }] : [
    { height: `${displayed}px`, overflow: "clip" }, { height: `${height}px`, overflow: "clip" },
  ], { duration: reduced ? 80 : 160, easing: "cubic-bezier(.2,0,0,1)" });
  c6ToolReveals.set(body, animation);
  animation.onfinish = animation.oncancel = () => {
    if (c6ToolReveals.get(body) === animation) c6ToolReveals.delete(body);
  };
};

// A native press owns its target through the compatibility click. Content can
// keep streaming, but resize-follow must not move that target before dispatch.
// This owner is independent of c6StopGlide's short-lived animation listeners.
window.c6FollowPress = (element, change) => {
  let held = false, disposed = false, timer, touches = 0, opener;
  let pageLive = true, blurred = false;
  const pointers = new Map();
  // Bound the no-click case after release (e.g. a scroll/longpress), never
  // expire a contact that is still down. Ordinary clicks finish sooner.
  const clickWait = 500;
  const available = () => pageLive && !blurred && !document.hidden;
  const clearTimer = () => { clearTimeout(timer); timer = undefined; };
  const finish = flush => {
    clearTimer();
    held = false;
    pointers.clear();
    touches = 0;
    opener = undefined;
    untrack();
    // Logical completion must reach a live owner even while detached. Rust
    // separately guards geometry; hide/pagehide discard, never flush old work.
    if (!disposed) change(false, available(), flush);
  };
  const release = delay => {
    clearTimer();
    timer = setTimeout(() => finish(true), delay);
  };
  const begin = event => {
    if (disposed || !available()) return false;
    if (!held) {
      if (event.type === 'pointerdown' && (event.button !== 0 || !event.isPrimary)) return false;
      if (event.type === 'touchstart' && event.touches.length !== 1) return false;
      const target = event.target instanceof Element ? event.target : event.target?.parentElement;
      const control = target?.closest('button, a[href], summary, input, textarea, select, label, [role="button"], [role="link"], [role="menuitem"], [contenteditable]:not([contenteditable="false"])');
      if (!control || !element.contains(control) || control.matches(':disabled, [aria-disabled="true"]')) return false;
      opener = control;
    }
    clearTimer();
    if (!held) {
      held = true;
      track();
      change(true, true, false);
      // Give native anchoring back while intrinsic rows above the opener
      // settle. This also freezes an active inverse at its displayed position.
      c6StopGlide(element);
    }
    return true;
  };
  const down = event => { if (begin(event)) pointers.set(event.pointerId, event.pointerType); };
  const up = event => {
    if (!held) return;
    pointers.delete(event.pointerId);
    if (!pointers.size && !touches) release(event.type === 'pointercancel' ? 0 : clickWait);
  };
  // A mouse/pen released outside the window may next appear only as a hover.
  const move = event => { if (event.pointerType !== 'touch' && event.buttons === 0 && pointers.has(event.pointerId)) up(event); };
  const touch = event => {
    if (event.type === 'touchstart' ? !begin(event) : !held) return;
    touches = event.touches.length;
    // Pointer cancellation can precede native scrolling. Touch events retain
    // ownership until the last finger leaves, including fingers added outside.
    if (!touches) for (const [id, type] of pointers) if (type === 'touch') pointers.delete(id);
    if (!pointers.size && !touches) release(event.type === 'touchcancel' ? 0 : clickWait);
  };
  const click = event => {
    // Capture sees even stopped clicks; the timer runs AFTER delegated/bubble
    // handlers. Never flush at pointerup/touchend ahead of a delayed click.
    if (held && !pointers.size && !touches && event.isTrusted && event.detail > 0 && opener?.contains(event.target)) release(0);
  };
  // Contextmenu is deliberately not a terminal event: the longpress contact
  // may still be down. Its actual up/cancel uses the bounded no-click fallback.
  const blur = () => { blurred = true; finish(false); };
  const focus = () => { if (blurred) { blurred = false; finish(false); } };
  const hide = () => { pageLive = false; finish(false); };
  const show = () => { pageLive = true; finish(false); };
  const visibility = () => finish(false);
  const listeners = { pointerdown: down, pointerup: up, pointercancel: up, pointermove: move,
    touchstart: touch, touchend: touch, touchcancel: touch, click };
  const track = () => {
    for (const [type, listener] of Object.entries(listeners)) document.addEventListener(type, listener, { capture: true, passive: true });
    addEventListener('blur', blur);
  };
  const untrack = () => {
    for (const [type, listener] of Object.entries(listeners)) document.removeEventListener(type, listener, true);
    removeEventListener('blur', blur);
  };
  // Lifecycle suspension outlives a press: pagehide need not set hidden yet.
  // A visible resume requests fresh geometry, never a saved correction target.
  addEventListener('pagehide', hide);
  addEventListener('pageshow', show);
  addEventListener('focus', focus);
  document.addEventListener('visibilitychange', visibility);
  element.addEventListener('pointerdown', down, { capture: true, passive: true });
  element.addEventListener('touchstart', touch, { capture: true, passive: true });
  // Observer replacement can retain a suspended Rust owner; always handshake.
  change(false, available(), false);
  return () => {
    disposed = true;
    held = false;
    pointers.clear();
    clearTimer();
    untrack();
    removeEventListener('pagehide', hide);
    removeEventListener('pageshow', show);
    removeEventListener('focus', focus);
    document.removeEventListener('visibilitychange', visibility);
    element.removeEventListener('pointerdown', down, true);
    element.removeEventListener('touchstart', touch, true);
    c6StopGlide(element);
  };
};

// Commit the real scroll range once, then translate the reading column back
// to its previous picture. WAAPI glides that inverse away on the compositor;
// there is no per-frame JS, scroll write, layout measurement or content scale.
// The containing row clips overflow so this visual cannot enlarge scrollHeight.
window.c6GlideToEnd = (() => {
  const active = new Map();
  const settled = new WeakMap();
  const anchoring = new WeakMap();
  const rowWatches = new WeakMap();
  const duration = 200;
  const inputs = ["wheel", "touchstart", "touchmove", "pointerdown"];
  const scrollKeys = new Set(["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End", " ", "Spacebar"]);
  // Invert x(t) for the approved curve, then evaluate y(t). Its derivative
  // stays positive; six bounded Newton steps avoid a per-frame search/table.
  const ease = progress => {
    let t = progress;
    for (let i = 0; i < 6; i++) {
      t -= (((1.6 * t - 1.2) * t + .6) * t - progress) / ((4.8 * t - 2.4) * t + .6);
      t = Math.max(0, Math.min(1, t));
    }
    return t * t * (3 - 2 * t);
  };
  const rememberRows = element => {
    if (rowWatches.has(element)) return;
    const dispose = () => {
      sizes.disconnect(); changes.disconnect(); rowWatches.delete(element);
    };
    const sizes = new ResizeObserver(entries => {
      if (!element.isConnected) { dispose(); return; }
      for (const { target, contentRect } of entries) {
        // Explicit fallback, not `auto`: the engine's remembered auto size
        // can predate a width change while the row was forced visible in the
        // live tail. Keep that real size when it leaves the tail, too.
        const height = `${contentRect.height}px`;
        if (target.style.containIntrinsicBlockSize !== height) target.style.containIntrinsicBlockSize = height;
      }
    });
    const visit = (node, action) => {
      if (!(node instanceof Element)) return;
      if (node.matches('.c6-transcript-row')) action(node);
      for (const row of node.querySelectorAll('.c6-transcript-row')) action(row);
    };
    const changes = new MutationObserver(records => {
      if (!element.isConnected) { dispose(); return; }
      for (const record of records) {
        for (const node of record.removedNodes) visit(node, row => sizes.unobserve(row));
        for (const node of record.addedNodes) visit(node, row => sizes.observe(row));
      }
    });
    visit(element, row => sizes.observe(row));
    changes.observe(element, { childList: true, subtree: true });
    rowWatches.set(element, dispose);
  };
  const offsetAt = (state, time) => {
    if (state.curve.length === 2) return state.curve[0].value * (1 - ease(Math.min(1, Math.max(0, time / duration))));
    const right = state.curve.findIndex(point => point.time >= time);
    if (right <= 0) return right === 0 ? state.curve[0].value : 0;
    const a = state.curve[right - 1], b = state.curve[right];
    return a.value + (b.value - a.value) * (time - a.time) / (b.time - a.time);
  };
  const clear = element => {
    const state = active.get(element);
    if (!state) return;
    active.delete(element);
    state.animation.onfinish = state.animation.oncancel = null;
    state.animation.cancel();
    for (const type of inputs) element.removeEventListener(type, state.input, true);
    element.removeEventListener("keydown", state.key, true);
  };
  const stop = element => {
    const state = active.get(element);
    const offset = state ? offsetAt(state, state.animation.currentTime || 0) : 0;
    clear(element);
    settled.delete(element);
    rowWatches.get(element)?.();
    if (anchoring.has(element)) {
      element.style.overflowAnchor = anchoring.get(element);
      anchoring.delete(element);
    }
    // Give native wheel/touch/key/scrollbar input the displayed position, not
    // the destination hidden behind the inverse. No geometry read is needed.
    if (state && element.isConnected) element.scrollTo({ top: state.target - offset, behavior: "instant" });
  };
  window.c6StopGlide = stop;
  addEventListener("c6-motion", () => {
    if (!c6Motion.reduced()) return;
    for (const [element, state] of active) {
      clear(element);
      if (element.isConnected) element.scrollTo({ top: state.target, behavior: "instant" });
    }
  });
  return element => {
    if (!element.isConnected) { stop(element); return; }
    rememberRows(element);
    // Native scroll anchoring must not compensate the same translated rows a
    // second time when their effects finish. Restore it when the reader stops
    // following, so late layout above an older reading position still anchors.
    if (!anchoring.has(element)) anchoring.set(element, element.style.overflowAnchor);
    element.style.overflowAnchor = 'none';
    const from = element.scrollTop;
    const height = element.scrollHeight;
    const viewport = element.clientHeight;
    const target = Math.max(0, height - viewport);
    const state = active.get(element);
    // Remember the prior end even after finishing: browser clamping can
    // already have changed scrollTop when content or a phone row shrinks.
    const delta = target - (state?.target ?? settled.get(element) ?? from);
    settled.set(element, target);
    const content = element.querySelector('[data-follow-content]');
    if (c6Motion.reduced() || !content || Math.abs(delta) > viewport) {
      clear(element);
      if (from !== target) element.scrollTo({ top: target, behavior: "instant" });
      return;
    }
    if (from !== target) element.scrollTo({ top: target, behavior: "instant" });
    if (delta === 0) return;
    const next = state || { motions: [] };
    next.target = target;
    // Keep the age of every correction: growth must not restart an old curve
    // or pull a new one into an almost-expired clock. Chromium can advertise
    // an accelerated layer for additive transforms yet freeze those pixels
    // during a blocked main thread. Present their summed trajectory as ONE
    // replace transform instead. Only overlapping curves need 2ms keyframes;
    // playback has no JS work, and each old contribution still expires on time.
    const previous = state?.animation;
    const elapsed = previous?.currentTime || 0;
    next.motions = next.motions.map(motion => ({ ...motion, age: motion.age + elapsed })).filter(motion => motion.age < duration);
    if (previous?.currentTime === null && next.motions.length) next.motions.at(-1).offset += delta;
    else next.motions.push({ offset: delta, age: 0 });
    const times = next.motions.length === 1 ? [0, duration] : [...new Set([
      ...Array.from({ length: 101 }, (_, i) => i * 2), ...next.motions.map(motion => duration - motion.age),
    ])].sort((a, b) => a - b);
    next.curve = times.map(time => ({ time, value: next.motions.reduce((sum, motion) =>
      sum + motion.offset * (1 - ease(Math.min(1, (motion.age + time) / duration))), 0) }));
    const keyframes = next.curve.map(point => ({ offset: point.time / duration, transform: `translateY(${point.value}px)` }));
    if (previous?.currentTime === null) { previous.effect.setKeyframes(keyframes); return; }
    const animation = content.animate(keyframes, {
      duration, easing: next.motions.length === 1 ? 'cubic-bezier(.2,0,0,1)' : 'linear', composite: 'replace',
    });
    next.animation = animation;
    if (previous) { previous.onfinish = previous.oncancel = null; previous.cancel(); }
    const finished = () => {
      if (active.get(element) === next && next.animation === animation) clear(element);
    };
    animation.onfinish = animation.oncancel = finished;
    if (state) return;
    next.input = () => { if (active.get(element) === next) stop(element); };
    next.key = event => {
      if (!scrollKeys.has(event.key)) return;
      if (event.target instanceof Element && event.target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) return;
      next.input();
    };
    active.set(element, next);
    for (const type of inputs) element.addEventListener(type, next.input, { capture: true, passive: true });
    element.addEventListener("keydown", next.key, true);
  };
})();
