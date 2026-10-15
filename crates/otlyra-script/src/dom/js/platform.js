// A media query the page can ask about. Answered from what we know, which is
// one window with no preference declared; the listener half exists so that a
// page that subscribes does not throw on a browser that never changes its
// answer.
defineGlobal('matchMedia', function matchMedia(query) {
  return {
    media: String(query),
    matches: false,
    onchange: null,
    addListener() {},
    removeListener() {},
    addEventListener() {},
    removeEventListener() {},
    dispatchEvent() {
      return false;
    },
  };
});

// Animation callbacks stay in the isolate until the host supplies a visible
// frame. Hiding the document keeps the registrations for its next visible frame.
const frameCallbacks = new Map();
let nextFrameHandle = 1;

// `onvisibilitychange` and `onresize` are ordinary event handlers; see
// `handlers.js`.

defineGlobal('requestAnimationFrame', function requestAnimationFrame(callback) {
  if (typeof callback !== 'function') {
    throw new TypeError('requestAnimationFrame expects a function');
  }
  const handle = nextFrameHandle++;
  frameCallbacks.set(handle, callback);
  // So the browser knows a frame is owed without asking the isolate.
  nativeFrameRequested();
  return handle;
});

defineGlobal('cancelAnimationFrame', function cancelAnimationFrame(handle) {
  frameCallbacks.delete(handle);
});

// `setTimeout` and its family are the engine's own, and they work because a
// scheduler is installed under them — see `crates/otlyra-script/src/timers.rs`.
// There is nothing to shim here: what was once a map and a single flush is a
// wheel the browser turns, so a page's deferred work happens when the page
// asked for it rather than all at once when the parse ends.
//
// `setImmediate` is Node's rather than the web's, and the engine ships it on
// the same scheduler. A page that feature-detects it will find it; nothing on
// the web depends on that.

// Load events run even in a background tab; its rendering callbacks wait.
let contentLoadedFired = false;
let loadEventsFired = false;

// `fireLoadEvents` is `true` for both events, `'dcl'` for DOMContentLoaded
// alone (the browser fires `load` once the document's pictures and sheets
// are in), and `false` while scripts the document waits for are still out.
defineGlobal('__otlyraFlushDeferred', function __otlyraFlushDeferred(fireLoadEvents) {
  let ran = 0;

  // The two events every page waits for. They come first because everything a
  // page defers is registered from one of them — and they happen once, when
  // the host says every script that was going to arrive has arrived.
  const due = [];
  if (fireLoadEvents && !contentLoadedFired) {
    contentLoadedFired = true;
    // A parsed `onmousemove` registers its listener only when an event finds
    // it, so its presence alone asks for moves.
    if (document.querySelector('[onmousemove], [onpointermove]')) nativePointerMovesWanted();
    // `DOMContentLoaded` bubbles and `load` does not, which is not a detail:
    // half the pages on the web listen for the first one on `window`, and
    // they hear it only because it travels up from the document.
    due.push([document, 'DOMContentLoaded', true]);
  }
  if (fireLoadEvents === true && !loadEventsFired) {
    loadEventsFired = true;
    due.push([globalThis, 'load', false]);
  }
  if (due.length > 0) {
    for (const [target, type, bubbles] of due) {
      try {
        target.dispatchEvent(new Event(type, { bubbles }));
      } catch (error) {
        console.error(error);
      }
      ran++;
    }
  }

  return ran + __otlyraRunFrame(pageTimeMetric(0));
});

// One turn of the frame loop: every callback registered up to now, and none
// registered by them — a callback that asks for another frame is asking for
// the *next* one, and running it here would be a loop with no display in it.
defineGlobal('__otlyraRunFrame', function __otlyraRunFrame(timestamp) {
  if (document.hidden) return 0;
  const frames = [...frameCallbacks.entries()];
  frameCallbacks.clear();
  for (const [, callback] of frames) {
    try {
      callback(Number(timestamp) || 0);
    } catch (error) {
      console.error(error);
    }
  }
  return frames.length;
});

// Whether a frame is owed, for a host deciding whether to enter the isolate
// at all.
defineGlobal('__otlyraFramesPending', function __otlyraFramesPending() {
  return frameCallbacks.size;
});

// Geometry, which needs layout and has none to read yet. Zeroes rather than
// an exception: a page that measures an element and gets zero lays itself out
// as if the element were empty, which is wrong but renderable, where a throw
// takes the rest of the script with it.
define(Element.prototype, 'getBoundingClientRect', function getBoundingClientRect() {
  return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
});

if (typeof CustomEvent === 'undefined' && typeof Event !== 'undefined') {
  class CustomEventShim extends Event {
    constructor(type, options) {
      super(type, options);
      this.detail = options && 'detail' in options ? options.detail : null;
    }
  }
  defineGlobal('CustomEvent', CustomEventShim);
}

if (typeof navigator === 'object' && navigator) {
  // It cannot send, and says so rather than pretending: a page that believes
  // its beacon left is a page that will not try the request another way.
  define(navigator, 'sendBeacon', () => false);
  define(navigator, 'language', 'en-US');
  define(navigator, 'languages', ['en-US']);
  define(navigator, 'onLine', true);
  define(navigator, 'cookieEnabled', true);
}

if (typeof performance === 'object' && performance) {
  define(performance, 'getEntriesByType', () => []);
  define(performance, 'getEntriesByName', () => []);
  define(performance, 'getEntries', () => []);
  define(performance, 'mark', () => undefined);
  define(performance, 'measure', () => undefined);
  define(performance, 'timing', {});
  define(performance, 'navigation', { type: 0, redirectCount: 0 });
}

// An observer that never observes. The alternative is that a page which
// lazy-loads through one shows nothing at all.
class NoopObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
}
if (typeof IntersectionObserver === 'undefined') defineGlobal('IntersectionObserver', NoopObserver);
if (typeof MutationObserver === 'undefined') defineGlobal('MutationObserver', NoopObserver);
if (typeof ResizeObserver === 'undefined') defineGlobal('ResizeObserver', NoopObserver);

// A user-input event the browser dispatches: what the reader did, as the event
// a page listens for. The browser picks the target (`document.__otlyraInputTarget`)
// and says whether the page cancelled it, which decides whether a link is
// followed, a field focused or a selection started.
const INPUT_EVENTS = {
  pointerdown: [PointerEvent, true, true],
  pointerup: [PointerEvent, true, true],
  pointermove: [PointerEvent, true, true],
  pointerover: [PointerEvent, true, true],
  pointerout: [PointerEvent, true, true],
  pointerenter: [PointerEvent, false, false],
  pointerleave: [PointerEvent, false, false],
  mouseover: [MouseEvent, true, true],
  mouseout: [MouseEvent, true, true],
  mouseenter: [MouseEvent, false, false],
  mouseleave: [MouseEvent, false, false],
  mousedown: [MouseEvent, true, true],
  mouseup: [MouseEvent, true, true],
  mousemove: [MouseEvent, true, true],
  click: [PointerEvent, true, true],
  auxclick: [PointerEvent, true, true],
  dblclick: [MouseEvent, true, true],
  contextmenu: [PointerEvent, true, true],
  wheel: [WheelEvent, true, true],
  keydown: [KeyboardEvent, true, true],
  keyup: [KeyboardEvent, true, true],
  keypress: [KeyboardEvent, true, true],
  beforeinput: [InputEvent, true, true],
  input: [InputEvent, true, false],
  change: [Event, true, false],
  load: [Event, false, false],
  error: [Event, false, false],
  focus: [FocusEvent, false, false],
  blur: [FocusEvent, false, false],
  focusin: [FocusEvent, true, false],
  focusout: [FocusEvent, true, false],
};

defineGlobal('__otlyraDispatchInput', function __otlyraDispatchInput(type, init) {
  const target = document.__otlyraInputTarget || document.body || document.documentElement;
  const shape = INPUT_EVENTS[type];
  if (!target || !shape) return 0;
  const [Interface, bubbles, cancelable] = shape;
  const event = new Interface(type, {
    bubbles,
    cancelable,
    composed: true,
    view: globalThis,
    ...init,
  });
  // What the reader did, not what a script made up.
  try {
    Object.defineProperty(event, 'isTrusted', { value: true, enumerable: true });
  } catch {
    // An engine that keeps `isTrusted` to itself keeps it false.
  }
  target.dispatchEvent(event);
  return event.defaultPrevented ? 1 : 0;
});
