// `window` is the global object, and everything a page uses to say "the window
// I am in" is the same object. A page in no frame is its own top, has no opener
// and holds no frames.
// They are the window's own accessors, as WebIDL makes them: code that keeps
// `Object.getOwnPropertyDescriptor(window, 'closed').get` to call later finds
// a getter. `self`, `frames` and `parent` are [Replaceable]: assigning one
// replaces it with a plain value.
const windowAttribute = (name, get, set) => {
  Object.defineProperty(globalThis, name, { get, set, enumerable: true, configurable: true });
};
const replaceable = (name) => function (value) {
  Object.defineProperty(globalThis, name, { value, writable: true, enumerable: true, configurable: true });
};
windowAttribute('window', function window() { return globalThis; });
windowAttribute('top', function top() { return globalThis; });
for (const name of ['self', 'frames', 'parent']) {
  windowAttribute(name, { [name]() { return globalThis; } }[name], replaceable(name));
}
windowAttribute('frameElement', function frameElement() { return null; });
windowAttribute('closed', function closed() { return false; });
let windowOpener = null;
windowAttribute('opener', function opener() { return windowOpener; }, function opener(value) { windowOpener = value; });
let windowName = '';
windowAttribute('name', function name() { return windowName; }, function name(value) { windowName = String(value); });

// Page turns use the browser's monotonic clock. Bare ScriptHost embedders
// without a document loan retain the engine's fallback performance clock.
const nativeTimeMetric = Document.__timeMetric;
delete Document.__timeMetric;
const performanceObject = globalThis.performance;
const fallbackPerformanceNow = Performance.prototype.now;
const fallbackPerformanceOrigin = Object.getOwnPropertyDescriptor(Performance.prototype, 'timeOrigin').get;
function pageTimeMetric(which) {
  const value = nativeTimeMetric(which);
  if (value === value) return value;
  return which === 0 ? fallbackPerformanceNow.call(performanceObject) : fallbackPerformanceOrigin.call(performanceObject);
}
function performanceReceiver(value) {
  if (value !== performanceObject) throw new TypeError('Illegal invocation');
}
Object.defineProperties(Performance.prototype, Object.getOwnPropertyDescriptors({
  now() { performanceReceiver(this); return pageTimeMetric(0); },
  get timeOrigin() { performanceReceiver(this); return pageTimeMetric(1); },
  toJSON() { performanceReceiver(this); return { timeOrigin: pageTimeMetric(1) }; },
}));

const nativeWindowMetric = Document.__windowMetric;
delete Document.__windowMetric;
for (const [name, index] of [['innerWidth', 0], ['innerHeight', 1], ['devicePixelRatio', 2]]) {
  Object.defineProperty(globalThis, name, {
    get() { return nativeWindowMetric(index); },
    set(value) {
      Object.defineProperty(globalThis, name, { value, writable: true, enumerable: true, configurable: true });
    },
    enumerable: true,
    configurable: true,
  });
}

// `screen`: the window's own size, as Firefox reports it when it resists
// fingerprinting. The page has no business knowing the display it is on, and
// the code that reads `screen.width` is choosing a layout for the window.
class Screen {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
}
const screenObject = Object.create(Screen.prototype);
for (const [name, index] of [['width', 0], ['height', 1], ['availWidth', 0], ['availHeight', 1]]) {
  Object.defineProperty(Screen.prototype, name, {
    get() { return Math.round(nativeWindowMetric(index)); },
    enumerable: true,
    configurable: true,
  });
}
for (const [name, value] of [['colorDepth', 24], ['pixelDepth', 24], ['availLeft', 0], ['availTop', 0]]) {
  Object.defineProperty(Screen.prototype, name, { get() { return value; }, enumerable: true, configurable: true });
}
defineGlobal('Screen', Screen);
defineGlobal('screen', screenObject);

// The global's interface. The global object is the engine's own; it is put
// on `Window.prototype`, so what a page reads from `Window.prototype` or
// `EventTarget.prototype` is what the window has.
class Window extends EventTarget {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
}
defineGlobal('Window', Window);
Object.setPrototypeOf(globalThis, Window.prototype);

// `document` is fetched on first use rather than now: this script is
// evaluated while the isolate is being built, and at that moment there is no
// page yet. Once fetched it is kept — the wrapper table hands back the same
// object anyway, and this saves the trip.
let documentSingleton = null;
Object.defineProperty(globalThis, 'document', {
  get() {
    if (documentSingleton === null) documentSingleton = nativeDocument();
    return documentSingleton;
  },
  set(value) {
    documentSingleton = value;
  },
  enumerable: false,
  configurable: true,
});
