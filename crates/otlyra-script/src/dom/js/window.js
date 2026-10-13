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
