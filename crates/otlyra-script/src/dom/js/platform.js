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

// The document's event-handler property shares the normal listener route.
// Replacing a callback retains its position among registered listeners.
let visibilityHandler = null;
const visibilityListener = function (event) {
  visibilityHandler.call(this, event);
};
Object.defineProperty(Document.prototype, 'onvisibilitychange', {
  configurable: true,
  enumerable: true,
  get() { return visibilityHandler; },
  set(callback) {
    const next = typeof callback === 'function' ? callback : null;
    if (visibilityHandler === null && next !== null) {
      this.addEventListener('visibilitychange', visibilityListener);
    } else if (visibilityHandler !== null && next === null) {
      this.removeEventListener('visibilitychange', visibilityListener);
    }
    visibilityHandler = next;
  },
});

let resizeHandler = null;
const resizeListener = function(event) { if (resizeHandler !== null) return resizeHandler.call(this, event); };
Object.defineProperty(globalThis, 'onresize', {
  configurable: true,
  enumerable: true,
  get() { return resizeHandler; },
  set(callback) {
    const next = typeof callback === 'function' ? callback : null;
    if (resizeHandler === null && next !== null) {
      this.addEventListener('resize', resizeListener);
    } else if (resizeHandler !== null && next === null) {
      this.removeEventListener('resize', resizeListener);
    }
    resizeHandler = next;
  },
});

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
let loadEventsFired = false;

defineGlobal('__otlyraFlushDeferred', function __otlyraFlushDeferred(fireLoadEvents) {
  let ran = 0;

  // The two events every page waits for. They come first because everything a
  // page defers is registered from one of them — and they happen once, when
  // the host says every script that was going to arrive has arrived.
  if (fireLoadEvents && !loadEventsFired) {
    loadEventsFired = true;
    // `DOMContentLoaded` bubbles and `load` does not, which is not a detail:
    // half the pages on the web listen for the first one on `window`, and
    // they hear it only because it travels up from the document.
    for (const [target, type, bubbles] of [
      [document, 'DOMContentLoaded', true],
      [globalThis, 'load', false],
    ]) {
      try {
        target.dispatchEvent(new Event(type, { bubbles }));
      } catch (error) {
        console.error(error);
      }
      ran++;
    }
  }

  return ran + __otlyraRunFrame(0);
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
