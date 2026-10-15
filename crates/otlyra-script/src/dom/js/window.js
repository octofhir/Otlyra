// `window` is the global object, and everything a page uses to say "the window
// I am in" is the same object. A page in no frame is its own top, has no opener
// and holds no frames.
defineGlobal('window', globalThis);
defineGlobal('self', globalThis);
defineGlobal('top', globalThis);
defineGlobal('parent', globalThis);
defineGlobal('frames', globalThis);
define(globalThis, 'opener', null);
define(globalThis, 'frameElement', null);
define(globalThis, 'closed', false);
define(globalThis, 'name', '');

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

// The global's interface. The global object is the engine's own and keeps its
// prototype; `instanceof Window` answers for it rather than for its chain.
class Window extends EventTarget {
  constructor() {
    throw new TypeError('Illegal constructor');
  }

  static [Symbol.hasInstance](value) {
    return value === globalThis;
  }
}
defineGlobal('Window', Window);

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
