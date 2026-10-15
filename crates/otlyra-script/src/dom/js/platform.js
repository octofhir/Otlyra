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
    if (document.querySelector('[onscroll]')) nativeScrollEventsWanted();
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
  // The rendering step's observers, after the frame's own callbacks.
  return frames.length + updateGeometryObservers(Number(timestamp) || 0);
});

// Whether a frame is owed, for a host deciding whether to enter the isolate
// at all.
defineGlobal('__otlyraFramesPending', function __otlyraFramesPending() {
  return frameCallbacks.size;
});

// Geometry, read from the boxes the last frame drew. Script that changes the
// tree and measures in the same turn reads the frame before its change; an
// element that was not drawn measures zero rather than throwing, which a page
// lays out as if it were empty — wrong, but renderable.
const clientRectOf = (element) => {
  const edges = element.__otlyraClientRect();
  const [left, top, right, bottom] = edges || [0, 0, 0, 0];
  return {
    x: left,
    y: top,
    left,
    top,
    right,
    bottom,
    width: right - left,
    height: bottom - top,
    toJSON() {
      return { x: left, y: top, left, top, right, bottom, width: right - left, height: bottom - top };
    },
  };
};
method(Element.prototype, 'getBoundingClientRect', function getBoundingClientRect() {
  return clientRectOf(this);
});
method(Element.prototype, 'getClientRects', function getClientRects() {
  return this.__otlyraClientRect() ? [clientRectOf(this)] : [];
});
const isScrollingRoot = (element) =>
  element === document.documentElement || element === document.body;
for (const [name, measure] of [
  ['offsetWidth', (rect) => rect.width],
  ['offsetHeight', (rect) => rect.height],
  ['offsetLeft', (rect) => rect.left + scrollPosition()[0]],
  ['offsetTop', (rect) => rect.top + scrollPosition()[1]],
]) {
  accessor(HTMLElement.prototype, name, function () {
    return Math.round(measure(clientRectOf(this)));
  });
}
accessor(HTMLElement.prototype, 'offsetParent', function () {
  return this.isConnected && this !== document.body ? document.body : null;
});
for (const [name, axis] of [['clientWidth', 0], ['clientHeight', 1]]) {
  accessor(Element.prototype, name, function () {
    if (this === document.documentElement) return axis === 0 ? innerWidth : innerHeight;
    const rect = clientRectOf(this);
    return Math.round(axis === 0 ? rect.width : rect.height);
  });
}
for (const [name, axis] of [['scrollWidth', 0], ['scrollHeight', 1]]) {
  accessor(Element.prototype, name, function () {
    if (isScrollingRoot(this)) {
      const extent = document.__otlyraExtent[axis] + scrollPosition()[axis];
      return Math.round(Math.max(extent, axis === 0 ? innerWidth : innerHeight));
    }
    const rect = clientRectOf(this);
    return Math.round(axis === 0 ? rect.width : rect.height);
  });
}

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

// An observer that never observes: what is left of MutationObserver until
// the tree reports its own changes.
class NoopObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
}
if (typeof MutationObserver === 'undefined') defineGlobal('MutationObserver', NoopObserver);

// IntersectionObserver and ResizeObserver, answered from the boxes the last
// frame drew, in the rendering step after the animation-frame callbacks — as
// the HTML event loop runs them. Observing asks for a frame, so the first
// notification comes without waiting for the page to change; afterwards the
// browser runs the step whenever a frame or a scroll moved what was drawn.
const geometryObservers = new Set();

const parseRootMargin = (margin) => {
  const parts = String(margin || '0px').trim().split(/\s+/).map((part) => {
    const value = Number.parseFloat(part);
    if (!Number.isFinite(value)) return () => 0;
    return part.endsWith('%')
      ? (size) => (size * value) / 100
      : () => value;
  });
  const [top, right = top, bottom = top, left = right] = parts;
  return { top, right, bottom, left };
};

class IntersectionObserverImpl {
  constructor(callback, options = {}) {
    if (typeof callback !== 'function') {
      throw new TypeError('IntersectionObserver needs a callback');
    }
    this.callback = callback;
    this.root = options && options.root ? options.root : null;
    this.rootMargin = String((options && options.rootMargin) || '0px');
    const threshold = options && options.threshold !== undefined ? options.threshold : 0;
    this.thresholds = (Array.isArray(threshold) ? threshold : [threshold])
      .map(Number)
      .sort((a, b) => a - b);
    this.targets = new Map();
    this.records = [];
  }
  observe(target) {
    if (!this.targets.has(target)) this.targets.set(target, null);
    geometryObservers.add(this);
    nativeGeometryWanted();
    nativeFrameRequested();
  }
  unobserve(target) {
    this.targets.delete(target);
  }
  disconnect() {
    this.targets.clear();
    geometryObservers.delete(this);
  }
  takeRecords() {
    const records = this.records;
    this.records = [];
    return records;
  }
  update(time) {
    const margin = parseRootMargin(this.rootMargin);
    const rootRect = this.root
      ? clientRectOf(this.root)
      : { left: 0, top: 0, right: innerWidth, bottom: innerHeight, width: innerWidth, height: innerHeight };
    const width = rootRect.right - rootRect.left;
    const height = rootRect.bottom - rootRect.top;
    const bounds = {
      left: rootRect.left - margin.left(width),
      top: rootRect.top - margin.top(height),
      right: rootRect.right + margin.right(width),
      bottom: rootRect.bottom + margin.bottom(height),
    };
    for (const [target, previous] of this.targets) {
      const drawn = target.isConnected && target.__otlyraClientRect() !== null;
      const rect = clientRectOf(target);
      const left = Math.max(rect.left, bounds.left);
      const top = Math.max(rect.top, bounds.top);
      const right = Math.min(rect.right, bounds.right);
      const bottom = Math.min(rect.bottom, bounds.bottom);
      const isIntersecting = drawn && right >= left && bottom >= top;
      const area = rect.width * rect.height;
      const ratio = !isIntersecting ? 0 : area > 0 ? ((right - left) * (bottom - top)) / area : 1;
      const index = this.thresholds.filter((value) => ratio >= value && (isIntersecting || value > 0)).length;
      const state = `${isIntersecting}:${index}`;
      if (state === previous) continue;
      this.targets.set(target, state);
      const intersection = isIntersecting
        ? { x: left, y: top, left, top, right, bottom, width: right - left, height: bottom - top }
        : { x: 0, y: 0, left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0 };
      this.records.push({
        target,
        time,
        isIntersecting,
        intersectionRatio: ratio,
        boundingClientRect: rect,
        intersectionRect: intersection,
        rootBounds: { x: bounds.left, y: bounds.top, ...bounds, width: bounds.right - bounds.left, height: bounds.bottom - bounds.top },
      });
    }
    const records = this.takeRecords();
    if (records.length > 0) this.callback(records, this);
    return records.length;
  }
}
Object.defineProperty(IntersectionObserverImpl, 'name', { value: 'IntersectionObserver' });
defineGlobal('IntersectionObserver', IntersectionObserverImpl);

class ResizeObserverImpl {
  constructor(callback) {
    if (typeof callback !== 'function') {
      throw new TypeError('ResizeObserver needs a callback');
    }
    this.callback = callback;
    this.targets = new Map();
  }
  observe(target) {
    if (!this.targets.has(target)) this.targets.set(target, null);
    geometryObservers.add(this);
    nativeGeometryWanted();
    nativeFrameRequested();
  }
  unobserve(target) {
    this.targets.delete(target);
  }
  disconnect() {
    this.targets.clear();
    geometryObservers.delete(this);
  }
  update() {
    const entries = [];
    for (const [target, previous] of this.targets) {
      const rect = clientRectOf(target);
      const size = `${rect.width}x${rect.height}`;
      if (size === previous) continue;
      this.targets.set(target, size);
      const box = { inlineSize: rect.width, blockSize: rect.height };
      entries.push({
        target,
        contentRect: { x: 0, y: 0, left: 0, top: 0, right: rect.width, bottom: rect.height, width: rect.width, height: rect.height },
        borderBoxSize: [box],
        contentBoxSize: [box],
        devicePixelContentBoxSize: [box],
      });
    }
    if (entries.length > 0) this.callback(entries, this);
    return entries.length;
  }
}
Object.defineProperty(ResizeObserverImpl, 'name', { value: 'ResizeObserver' });
defineGlobal('ResizeObserver', ResizeObserverImpl);

const updateGeometryObservers = (time) => {
  if (geometryObservers.size === 0) return 0;
  // Nothing drawn yet: the browser runs this again once something is.
  const [right, bottom] = document.__otlyraExtent;
  if (right === 0 && bottom === 0) return 0;
  let notified = 0;
  for (const observer of [...geometryObservers]) {
    try {
      notified += observer.update(time);
    } catch (error) {
      console.error(error);
    }
  }
  return notified;
};

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
  submit: [SubmitEvent, true, true],
  scroll: [Event, true, false],
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

// WebDriver BiDi's `script.evaluate` and `script.callFunction`, for an
// automation client: run in the page's realm, answer with a RemoteValue.
// A promise is awaited across the microtask checkpoint that ends the turn;
// the browser takes the settled answer in a second turn.
let bidiAnswer = null;

const bidiSerialize = (value, depth, seen) => {
  switch (typeof value) {
    case 'undefined':
      return { type: 'undefined' };
    case 'string':
      return { type: 'string', value };
    case 'boolean':
      return { type: 'boolean', value };
    case 'bigint':
      return { type: 'bigint', value: value.toString() };
    case 'symbol':
      return { type: 'symbol' };
    case 'function':
      return { type: 'function' };
    case 'number':
      if (Number.isNaN(value)) return { type: 'number', value: 'NaN' };
      if (Object.is(value, -0)) return { type: 'number', value: '-0' };
      if (!Number.isFinite(value)) {
        return { type: 'number', value: value > 0 ? 'Infinity' : '-Infinity' };
      }
      return { type: 'number', value };
    default:
      break;
  }
  if (value === null) return { type: 'null' };
  if (value === globalThis) return { type: 'window' };
  const deeper = depth < 3 && !seen.has(value);
  if (deeper) seen.add(value);
  const child = (item) => bidiSerialize(item, depth + 1, seen);
  if (Array.isArray(value)) {
    return deeper ? { type: 'array', value: value.map(child) } : { type: 'array' };
  }
  if (typeof Node === 'function' && value instanceof Node) {
    const node = { nodeType: value.nodeType, childNodeCount: value.childNodes.length };
    if (value.nodeType === 1) {
      node.localName = value.localName;
      node.namespaceURI = value.namespaceURI;
      node.attributes = Object.fromEntries(
        [...value.attributes].map((attribute) => [attribute.name, attribute.value]),
      );
    } else if (value.nodeValue !== null) {
      node.nodeValue = value.nodeValue;
    }
    return { type: 'node', value: node };
  }
  if (value instanceof Error) return { type: 'error' };
  if (value instanceof Promise) return { type: 'promise' };
  if (value instanceof RegExp) {
    return { type: 'regexp', value: { pattern: value.source, flags: value.flags } };
  }
  if (value instanceof Date) return { type: 'date', value: value.toISOString() };
  if (value instanceof Map) {
    return deeper
      ? { type: 'map', value: [...value].map(([key, item]) => [typeof key === 'string' ? key : child(key), child(item)]) }
      : { type: 'map' };
  }
  if (value instanceof Set) {
    return deeper ? { type: 'set', value: [...value].map(child) } : { type: 'set' };
  }
  return deeper
    ? { type: 'object', value: Object.keys(value).map((key) => [key, child(value[key])]) }
    : { type: 'object' };
};

const bidiDeserialize = (local) => {
  if (local === null || typeof local !== 'object') return local;
  switch (local.type) {
    case 'undefined':
      return undefined;
    case 'null':
      return null;
    case 'string':
    case 'boolean':
      return local.value;
    case 'number':
      return local.value === '-0' ? -0 : Number(local.value);
    case 'bigint':
      return BigInt(local.value);
    case 'array':
      return (local.value || []).map(bidiDeserialize);
    case 'object': {
      const object = {};
      for (const [key, item] of local.value || []) {
        object[typeof key === 'string' ? key : bidiDeserialize(key)] = bidiDeserialize(item);
      }
      return object;
    }
    case 'date':
      return new Date(local.value);
    case 'regexp':
      return new RegExp(local.value.pattern, local.value.flags);
    default:
      throw new TypeError(`argument of type ${local.type} is not supported`);
  }
};

const bidiSuccess = (value) => ({ type: 'success', result: bidiSerialize(value, 0, new Set()) });
const bidiException = (error) => ({
  type: 'exception',
  exceptionDetails: {
    text: String(error && error.message !== undefined ? `${error.name}: ${error.message}` : error),
    lineNumber: 0,
    columnNumber: 0,
    exception: bidiSerialize(error, 0, new Set()),
    stackTrace: { callFrames: [] },
  },
});

// `run` produces the value; 1 when the answer is ready, 0 while a promise is
// still to settle.
const bidiRun = (run, awaitPromise) => {
  bidiAnswer = null;
  let value;
  try {
    value = run();
  } catch (error) {
    bidiAnswer = bidiException(error);
    return 1;
  }
  if (awaitPromise && value !== null && typeof value === 'object' && typeof value.then === 'function') {
    Promise.resolve(value).then(
      (settled) => {
        bidiAnswer = bidiSuccess(settled);
      },
      (error) => {
        bidiAnswer = bidiException(error);
      },
    );
    return 0;
  }
  bidiAnswer = bidiSuccess(value);
  return 1;
};

defineGlobal('__otlyraBidiEvaluate', function __otlyraBidiEvaluate(expression, awaitPromise) {
  return bidiRun(() => (0, eval)(expression), awaitPromise);
});

defineGlobal('__otlyraBidiCall', function __otlyraBidiCall(declaration, args, self, awaitPromise) {
  return bidiRun(() => {
    const callee = (0, eval)(`(${declaration})`);
    return callee.apply(bidiDeserialize(JSON.parse(self)), JSON.parse(args).map(bidiDeserialize));
  }, awaitPromise);
});

defineGlobal('__otlyraBidiTake', function __otlyraBidiTake() {
  const answer = bidiAnswer;
  bidiAnswer = null;
  return answer === null ? '' : JSON.stringify(answer);
});

// The window's scroll position, which the browser settles and script asks to
// change. Only the document scrolls: an element's own `scrollTop` is zero.
const scrollPosition = () => document.__otlyraScroll;
const toScrollNumber = (value, fallback) => {
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
};
for (const [name, axis] of [['scrollX', 0], ['pageXOffset', 0], ['scrollY', 1], ['pageYOffset', 1]]) {
  accessor(globalThis, name, function () {
    return scrollPosition()[axis];
  });
}
const scrollWindowTo = function scrollTo(first, second) {
  const [x, y] = scrollPosition();
  if (first !== null && typeof first === 'object') {
    nativeScrollTo(toScrollNumber(first.left, x), toScrollNumber(first.top, y));
  } else {
    nativeScrollTo(toScrollNumber(first, x), toScrollNumber(second, y));
  }
};
defineGlobal('scrollTo', scrollWindowTo);
defineGlobal('scroll', scrollWindowTo);
defineGlobal('scrollBy', function scrollBy(first, second) {
  const [x, y] = scrollPosition();
  if (first !== null && typeof first === 'object') {
    nativeScrollTo(x + toScrollNumber(first.left, 0), y + toScrollNumber(first.top, 0));
  } else {
    nativeScrollTo(x + toScrollNumber(first, 0), y + toScrollNumber(second, 0));
  }
});
if (!('scrollingElement' in Document.prototype)) {
  accessor(Document.prototype, 'scrollingElement', function () {
    return this.documentElement;
  });
}
for (const [name, axis] of [['scrollTop', 1], ['scrollLeft', 0]]) {
  accessor(
    Element.prototype,
    name,
    function () {
      return this === document.scrollingElement ? scrollPosition()[axis] : 0;
    },
    function (value) {
      if (this !== document.scrollingElement) return;
      const [x, y] = scrollPosition();
      const target = toScrollNumber(value, 0);
      nativeScrollTo(axis === 0 ? target : x, axis === 1 ? target : y);
    },
  );
}
