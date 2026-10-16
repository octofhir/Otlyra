// A media query the page can ask about. Answered from what we know, which is
// one window with no preference declared; the listener half exists so that a
// page that subscribes does not throw on a browser that never changes its
// answer.
// `matchMedia` (CSSOM View §4.2): a list whose `matches` is asked of the
// cascade's own media query evaluator against the window as it is now, so a
// page's script and its `@media` rules agree. Lists are held weakly; when
// the window changes, each one still alive whose answer changed hears
// `change`.
const nativeMatchMedia = take(Document, '__matchMedia');
const kMediaQueryList = Symbol('mediaQueryList');
const mediaQueryLists = new Set();
class MediaQueryList extends EventTarget {
  #media;
  #matches;

  constructor(token, media) {
    if (token !== kMediaQueryList) throw new TypeError('Illegal constructor');
    super();
    this.#media = media;
    this.#matches = nativeMatchMedia(media);
  }

  get media() {
    return this.#media;
  }

  get matches() {
    return nativeMatchMedia(this.#media);
  }

  addListener(callback) {
    if (typeof callback === 'function') this.addEventListener('change', callback);
  }

  removeListener(callback) {
    if (typeof callback === 'function') this.removeEventListener('change', callback);
  }

  // Whether the answer changed since this was last asked.
  [kMediaQueryList]() {
    const now = nativeMatchMedia(this.#media);
    if (now === this.#matches) return false;
    this.#matches = now;
    return true;
  }
}
defineHandler(MediaQueryList.prototype, 'change');
defineGlobal('MediaQueryList', MediaQueryList);

class MediaQueryListEvent extends Event {
  #media;
  #matches;

  constructor(type, init = {}) {
    super(type, init);
    this.#media = String(init.media ?? '');
    this.#matches = Boolean(init.matches);
  }

  get media() {
    return this.#media;
  }

  get matches() {
    return this.#matches;
  }
}
defineGlobal('MediaQueryListEvent', MediaQueryListEvent);

defineGlobal('matchMedia', function matchMedia(query) {
  if (arguments.length < 1) throw new TypeError("Failed to execute 'matchMedia' on 'Window': 1 argument required");
  const list = new MediaQueryList(kMediaQueryList, String(query));
  mediaQueryLists.add(new WeakRef(list));
  return list;
});

defineGlobal('__otlyraMediaChanged', function __otlyraMediaChanged() {
  for (const reference of [...mediaQueryLists]) {
    const list = reference.deref();
    if (!list) {
      mediaQueryLists.delete(reference);
      continue;
    }
    if (!list[kMediaQueryList]()) continue;
    try {
      list.dispatchEvent(new MediaQueryListEvent('change', { media: list.media, matches: list.matches }));
    } catch (error) {
      reportError(error);
    }
  }
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

// Cooperative scheduling's `requestIdleCallback`: work a page puts off until
// nothing else is waiting. There is no idle period to measure here, so a
// callback runs as a task of its own after the current one, with the 50 ms
// an idle browser gives it (Cooperative Scheduling §5.4) — the same order a
// quiet page sees, and never in the middle of another task.
const IDLE_BUDGET_MS = 50;
const idleTimers = new Map();
let nextIdleHandle = 1;
class IdleDeadline {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
}
Object.defineProperty(IdleDeadline.prototype, Symbol.toStringTag, {
  value: 'IdleDeadline',
  configurable: true,
});
defineGlobal('IdleDeadline', IdleDeadline);
defineGlobal('requestIdleCallback', function requestIdleCallback(callback, options) {
  if (typeof callback !== 'function') {
    throw new TypeError('requestIdleCallback expects a function');
  }
  const handle = nextIdleHandle++;
  const timer = setTimeout(() => {
    idleTimers.delete(handle);
    const started = performance.now();
    const deadline = Object.create(IdleDeadline.prototype, {
      didTimeout: { value: false, enumerable: true },
      timeRemaining: {
        value: () => Math.max(0, IDLE_BUDGET_MS - (performance.now() - started)),
      },
    });
    callback(deadline);
  }, 1);
  idleTimers.set(handle, timer);
  return handle;
});
defineGlobal('cancelIdleCallback', function cancelIdleCallback(handle) {
  const timer = idleTimers.get(handle);
  if (timer === undefined) return;
  clearTimeout(timer);
  idleTimers.delete(handle);
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
    if (document.querySelector('[onwheel]')) nativeWheelEventsWanted();
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

// An exception nothing caught, reported the way the HTML standard reports
// one: an `ErrorEvent` at the window, whose `onerror` gets the five
// arguments, and the console unless a listener cancelled the event.
// HTML §8.1.4.6 "report an exception": an `error` event on the window that
// carries the exception itself — error trackers read `event.error` — and,
// unless a listener cancels it, the exception in the console with where it
// was thrown, as a browser's console has it.
defineGlobal('__otlyraReportError', function __otlyraReportError(message, filename, thrown = null) {
  const event = new ErrorEvent('error', {
    message,
    filename,
    lineno: 0,
    colno: 0,
    error: thrown,
    cancelable: true,
  });
  let reported = true;
  try {
    reported = globalThis.dispatchEvent(event);
  } catch (error) {
    console.error(error);
  }
  if (reported) {
    const stack = thrown && typeof thrown === 'object' ? thrown.stack : undefined;
    console.error(typeof stack === 'string' && stack.includes('\n    at ') ? 'Uncaught ' + stack : message);
  }
  return reported;
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
  // Strings, as every browser has them, because sniffing code calls
  // `navigator.platform.indexOf(…)` without asking first. Empty is what the
  // standard allows a browser that does not say, and what Firefox's
  // `vendor` is.
  define(navigator, 'platform', '');
  define(navigator, 'vendor', '');
  define(navigator, 'maxTouchPoints', 0);
}

if (typeof performance === 'object' && performance) {
  // User Timing (W3C User Timing 3): marks and measures a page records and
  // reads back. No resource or navigation entries are recorded.
  const timingEntries = [];
  class PerformanceEntry {
    constructor(name, entryType, startTime, duration, detail) {
      this.name = name;
      this.entryType = entryType;
      this.startTime = startTime;
      this.duration = duration;
      this.detail = detail === undefined ? null : detail;
    }
    toJSON() {
      return { name: this.name, entryType: this.entryType, startTime: this.startTime, duration: this.duration };
    }
  }
  class PerformanceMark extends PerformanceEntry {}
  class PerformanceMeasure extends PerformanceEntry {}
  defineGlobal('PerformanceEntry', PerformanceEntry);
  defineGlobal('PerformanceMark', PerformanceMark);
  defineGlobal('PerformanceMeasure', PerformanceMeasure);
  // The names of `PerformanceTiming`'s attributes stand for those moments
  // (User Timing 3 §4.3, "convert a mark to a timestamp"). Each is taken as
  // the start of the navigation: nothing finer is recorded.
  const TIMING_NAMES = [
    'navigationStart', 'unloadEventStart', 'unloadEventEnd', 'redirectStart', 'redirectEnd',
    'fetchStart', 'domainLookupStart', 'domainLookupEnd', 'connectStart', 'connectEnd',
    'secureConnectionStart', 'requestStart', 'responseStart', 'responseEnd', 'domLoading',
    'domInteractive', 'domContentLoadedEventStart', 'domContentLoadedEventEnd', 'domComplete',
    'loadEventStart', 'loadEventEnd',
  ];
  const markTime = (name) => {
    if (name === undefined) return undefined;
    if (typeof name === 'number') return name;
    if (TIMING_NAMES.includes(String(name))) return 0;
    for (let index = timingEntries.length - 1; index >= 0; index--) {
      const entry = timingEntries[index];
      if (entry.entryType === 'mark' && entry.name === String(name)) return entry.startTime;
    }
    throw new DOMException("The mark '" + name + "' does not exist.", 'SyntaxError');
  };
  const clearEntries = (type, name) => {
    for (let index = timingEntries.length - 1; index >= 0; index--) {
      const entry = timingEntries[index];
      if (entry.entryType === type && (name === undefined || entry.name === String(name))) {
        timingEntries.splice(index, 1);
      }
    }
  };
  define(performance, 'mark', function mark(name, options = {}) {
    const startTime = options && options.startTime !== undefined ? Number(options.startTime) : performance.now();
    const entry = new PerformanceMark(String(name), 'mark', startTime, 0, options && options.detail);
    timingEntries.push(entry);
    return entry;
  });
  define(performance, 'measure', function measure(name, start, end) {
    let startTime = 0;
    let endTime = performance.now();
    let detail = null;
    if (start && typeof start === 'object') {
      if (start.start !== undefined) startTime = markTime(start.start);
      if (start.end !== undefined) endTime = markTime(start.end);
      if (start.duration !== undefined) endTime = startTime + Number(start.duration);
      detail = start.detail;
    } else {
      if (start !== undefined) startTime = markTime(start);
      if (end !== undefined) endTime = markTime(end);
    }
    const entry = new PerformanceMeasure(String(name), 'measure', startTime, endTime - startTime, detail);
    timingEntries.push(entry);
    return entry;
  });
  define(performance, 'clearMarks', (name) => clearEntries('mark', name));
  define(performance, 'clearMeasures', (name) => clearEntries('measure', name));
  define(performance, 'clearResourceTimings', () => undefined);
  define(performance, 'setResourceTimingBufferSize', () => undefined);
  define(performance, 'getEntries', () => timingEntries.slice());
  define(performance, 'getEntriesByType', (type) => timingEntries.filter((entry) => entry.entryType === String(type)));
  define(performance, 'getEntriesByName', (name, type) =>
    timingEntries.filter((entry) => entry.name === String(name) && (type === undefined || entry.entryType === String(type))),
  );
  const origin = typeof performance.timeOrigin === 'number' ? performance.timeOrigin : Date.now() - performance.now();
  define(performance, 'timing', Object.fromEntries(TIMING_NAMES.map((name) => [name, Math.round(origin)])));
  define(performance, 'navigation', { type: 0, redirectCount: 0 });
}

// MutationObserver (DOM §4.3). The tree keeps the registrations and the
// records (`otlyra-dom/src/observe.rs`), so a change is matched against the
// observers where it happens; this side numbers the observers, turns the
// records into `MutationRecord`s and calls the callbacks at the microtask
// checkpoint, which the host ends by calling
// `__otlyraNotifyMutationObservers`.
const nativeObserveMutations = take(Node.prototype, '__observeMutations');
const nativeDisconnectMutations = take(Document.prototype, '__disconnectMutations');
const nativeMutationNodes = take(Document.prototype, '__mutationNodes');
const nativeTakeMutations = take(Document.prototype, '__takeMutations');

const RECORD_KINDS = ['childList', 'attributes', 'characterData'];
class MutationRecord {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
}
const makeRecord = (fields) => Object.setPrototypeOf(fields, MutationRecord.prototype);

// Turn the tree's records into `MutationRecord`s, by observer number.
const takeRecordsOf = (observer) => {
  const nodes = nativeMutationNodes.call(document, observer);
  const fields = nativeTakeMutations.call(document, observer);
  const byObserver = new Map();
  let at = 0;
  for (let field = 0; field < fields.length; field += 10) {
    const added = Number(fields[field + 6]);
    const removed = Number(fields[field + 7]);
    const target = nodes[at++];
    const previousSibling = fields[field + 8] === '1' ? nodes[at++] : null;
    const nextSibling = fields[field + 9] === '1' ? nodes[at++] : null;
    const addedNodes = nodes.slice(at, at + added);
    at += added;
    const removedNodes = nodes.slice(at, at + removed);
    at += removed;
    const kind = RECORD_KINDS[Number(fields[field + 1])];
    const record = makeRecord({
      type: kind,
      target,
      addedNodes,
      removedNodes,
      previousSibling,
      nextSibling,
      attributeName: kind === 'attributes' ? fields[field + 2] : null,
      attributeNamespace: kind === 'attributes' && fields[field + 3] ? fields[field + 3] : null,
      oldValue: fields[field + 4] === '1' ? fields[field + 5] : null,
    });
    const id = Number(fields[field]);
    if (!byObserver.has(id)) byObserver.set(id, []);
    byObserver.get(id).push(record);
  }
  return byObserver;
};

const mutationObservers = new Map();
let nextObserverId = 1;
const observerIds = new WeakMap();

class MutationObserver {
  constructor(callback) {
    if (typeof callback !== 'function') {
      throw new TypeError("Failed to construct 'MutationObserver': parameter 1 is not of type 'Function'.");
    }
    const id = nextObserverId++;
    observerIds.set(this, { id, callback, targets: 0 });
  }

  observe(target, options = {}) {
    const state = observerIds.get(this);
    if (!state) throw new TypeError('Illegal invocation');
    if (!(target instanceof Node)) {
      throw new TypeError("Failed to execute 'observe' on 'MutationObserver': parameter 1 is not of type 'Node'.");
    }
    const init = options || {};
    let attributes = init.attributes;
    let characterData = init.characterData;
    const filter = init.attributeFilter;
    if (attributes === undefined && (init.attributeOldValue !== undefined || filter !== undefined)) {
      attributes = true;
    }
    if (characterData === undefined && init.characterDataOldValue !== undefined) characterData = true;
    if (!init.childList && !attributes && !characterData) {
      throw new TypeError("The options object must set at least one of 'attributes', 'characterData', or 'childList' to true.");
    }
    if (init.attributeOldValue && !attributes) {
      throw new TypeError("The options object may only set 'attributeOldValue' to true when 'attributes' is true or not present.");
    }
    if (filter !== undefined && !attributes) {
      throw new TypeError("The options object may only set 'attributeFilter' when 'attributes' is true or not present.");
    }
    if (init.characterDataOldValue && !characterData) {
      throw new TypeError("The options object may only set 'characterDataOldValue' to true when 'characterData' is true or not present.");
    }
    const flags =
      (init.childList ? 1 : 0) |
      (attributes ? 2 : 0) |
      (characterData ? 4 : 0) |
      (init.subtree ? 8 : 0) |
      (init.attributeOldValue ? 16 : 0) |
      (init.characterDataOldValue ? 32 : 0) |
      (filter !== undefined ? 64 : 0);
    const names = filter !== undefined ? Array.from(filter, String).join('\n') : '';
    nativeObserveMutations.call(target, state.id, flags, names);
    mutationObservers.set(state.id, this);
  }

  disconnect() {
    const state = observerIds.get(this);
    if (!state) return;
    nativeDisconnectMutations.call(document, state.id);
    mutationObservers.delete(state.id);
  }

  takeRecords() {
    const state = observerIds.get(this);
    if (!state) return [];
    return takeRecordsOf(state.id).get(state.id) || [];
  }
}
defineGlobal('MutationObserver', MutationObserver);
defineGlobal('MutationRecord', MutationRecord);
if (typeof WebKitMutationObserver === 'undefined') defineGlobal('WebKitMutationObserver', MutationObserver);

// DOM §4.3 "notify mutation observers": each observer with records, in the
// order the observers were made, gets them all in one call.
defineGlobal('__otlyraNotifyMutationObservers', function __otlyraNotifyMutationObservers() {
  const byObserver = takeRecordsOf(-1);
  for (const id of Array.from(byObserver.keys()).sort((a, b) => a - b)) {
    const observer = mutationObservers.get(id);
    if (!observer) continue;
    const records = byObserver.get(id);
    try {
      observerIds.get(observer).callback.call(observer, records, observer);
    } catch (error) {
      const message =
        error && typeof error === 'object' && 'message' in error
          ? 'Uncaught ' + (error.name || 'Error') + ': ' + error.message
          : 'Uncaught ' + String(error);
      __otlyraReportError(message, String(location.href), error);
    }
  }
});

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
