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
  addEventListener("storage", event => { if (event.key === KEY) apply(); });
  apply();
  return {
    get,
    set(choice) {
      try { localStorage.setItem(KEY, choice); } catch (_) {}
      apply();
    },
    reduced: () => document.documentElement.dataset.motion !== "full",
    off: () => document.documentElement.dataset.motion === "off",
  };
})();

// Start anywhere *inside* the phone drawer, not at Android's Back edge.
// Leave native vertical scrolling and taps alone until a clear left drag.
// Release returns control to the sheet's shared CSS tokens/presence state.
window.c6DrawerSwipe = (element, close) => {
  const phone = matchMedia("(width < 48rem), (height < 32rem)");
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
    drag = { id: event.pointerId, x: event.clientX, y: event.clientY, width: box.width, from: box.left, distance: 0, claimed: false };
  };
  const move = event => {
    if (!drag || drag.id !== event.pointerId) return;
    if (element.dataset.open !== "true" || !phone.matches) { reset(); return; }
    const dx = event.clientX - drag.x, dy = event.clientY - drag.y;
    if (!drag.claimed) {
      if (Math.max(Math.abs(dx), Math.abs(dy)) < 10) return;
      // Lock the *whole* gesture to scrolling/rightward motion, even if it
      // turns left later. A diagonal must not steal a nested vertical scroll.
      if (dx >= 0 || -dx <= Math.abs(dy) * 1.5) { reset(); return; }
      drag.claimed = true;
      suppressClick = true;
      element.setPointerCapture(event.pointerId);
      element.setAttribute("data-dragging", "");
    }
    event.preventDefault();
    drag.distance = Math.max(0, -dx);
    element.style.setProperty("--drawer-pull", `${Math.max(-drag.width, Math.min(0, drag.from + dx))}px`);
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

// Match c6-arrive: 200ms, cubic-bezier(.2,0,0,1). Only an invocation
// measures the scroll box. Frames write scroll position, never read layout.
window.c6GlideToEnd = (() => {
  const active = new Map();
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
  const stop = element => {
    const state = active.get(element);
    if (!state) return;
    active.delete(element);
    cancelAnimationFrame(state.frame);
    for (const type of inputs) element.removeEventListener(type, state.input, true);
    element.removeEventListener("keydown", state.key, true);
  };
  // Programmatic snaps/anchor jumps cancel without moving the current picture.
  window.c6StopGlide = stop;
  addEventListener("c6-motion", () => {
    if (!c6Motion.reduced()) return;
    for (const [element, state] of active) {
      stop(element);
      if (element.isConnected) element.scrollTo({ top: state.target, behavior: "instant" });
    }
  });
  return element => {
    if (!element.isConnected) { stop(element); return; }
    const from = element.scrollTop;
    const height = element.scrollHeight;
    const viewport = element.clientHeight;
    const target = Math.max(0, height - viewport);
    const gap = target - from;
    if (gap <= 0) { stop(element); return; }
    if (c6Motion.reduced() || gap > viewport) {
      stop(element);
      element.scrollTo({ top: target, behavior: "instant" });
      return;
    }
    // Repeated resize notifications for one destination must not keep moving
    // its finish line. A new target starts from the actual displayed position.
    if (active.get(element)?.target === target) return;
    stop(element);
    const state = { target, start: performance.now(), frame: 0 };
    state.input = () => { if (active.get(element) === state) stop(element); };
    state.key = event => {
      if (!scrollKeys.has(event.key)) return;
      if (event.target instanceof Element && event.target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) return;
      state.input();
    };
    active.set(element, state);
    for (const type of inputs) element.addEventListener(type, state.input, { capture: true, passive: true });
    element.addEventListener("keydown", state.key, true);
    const tick = time => {
      if (active.get(element) !== state) return;
      if (!element.isConnected) { stop(element); return; }
      const progress = Math.min(1, Math.max(0, (time - state.start) / duration));
      element.scrollTo({ top: from + gap * ease(progress), behavior: "instant" });
      if (progress === 1) stop(element);
      else state.frame = requestAnimationFrame(tick);
    };
    state.frame = requestAnimationFrame(tick);
  };
})();
