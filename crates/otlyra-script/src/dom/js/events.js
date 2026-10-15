// Listener registration. The callbacks live in a JavaScript map keyed by the
// target object, so the collector traces them; nothing about a listener is
// held on the Rust side, where it would be invisible to it.
//
// A strong Map rather than a WeakMap because the engine will not take a host
// object as a WeakMap key. Wrappers do have identity now — the same node is
// the same object every time it is handed out — which is what makes a table
// keyed on them work at all: a listener registered through one lookup is
// found through any other.
const listeners = new Map();

const listenersFor = (target, type, create) => {
  let byType = listeners.get(target);
  if (!byType) {
    if (!create) return undefined;
    byType = new Map();
    listeners.set(target, byType);
  }
  let list = byType.get(type);
  if (!list) {
    if (!create) return undefined;
    if (type === 'mousemove' || type === 'pointermove') nativePointerMovesWanted();
    if (type === 'scroll') nativeScrollEventsWanted();
    list = [];
    byType.set(type, list);
  }
  return list;
};

// The engine's `Event` keeps its state under registered symbols so that a
// host which owns the tree can dispatch through it — see
// `otter-web/src/web_bootstrap.js`. Reading them is how this knows a page
// called `stopPropagation`; writing them is how `event.target` reports what
// the event was aimed at rather than what is currently handling it.
const kStop = Symbol.for('otter.event.stopPropagation');
const kStopImmediate = Symbol.for('otter.event.stopImmediate');
const kTarget = Symbol.for('otter.event.target');
const kDispatch = Symbol.for('otter.event.dispatching');

const CAPTURING_PHASE = 1;
const AT_TARGET = 2;
const BUBBLING_PHASE = 3;

// `capture` may be a boolean or an options object, and the two spellings are
// the same listener: `addEventListener(t, f, true)` and `{capture: true}`
// remove each other.
const optionOf = (options, name) => {
  if (typeof options === 'boolean') return name === 'capture' ? options : false;
  return Boolean(options && typeof options === 'object' && options[name]);
};

// Everything an event travels through, target first, window last.
//
// Built from `parentNode`, so it is the tree as it stands at dispatch — which
// is what the specification says, and why a listener that moves the target
// still sees the path it was dispatched into.
const propagationPath = (target) => {
  const path = [target];
  if (target === globalThis) return path;
  let node = target;
  // A bounded walk: a tree with a cycle in it is a bug elsewhere, and this is
  // not the place to hang because of one.
  for (let step = 0; step < 4096; step++) {
    const parent = node.parentNode;
    if (!parent || path.includes(parent)) break;
    path.push(parent);
    node = parent;
  }
  if (!path.includes(globalThis)) path.push(globalThis);
  return path;
};

// Call the listeners one target has for this event, in registration order.
//
// The list is copied first: a listener that adds another must not have the
// new one run for the event already in flight, and one that removes a later
// listener must stop it running — which is why the copy is consulted for the
// order and the live list for whether an entry is still there.
const fire = (target, event, capturing) => {
  // Handlers are never capturing; a content attribute registers its handler
  // the first time an event it handles arrives (see `handlers.js`).
  if (!capturing) syncAttributeHandler(target, event.type);
  const list = listenersFor(target, event.type, false);
  if (!list || list.length === 0) return;
  event.currentTarget = target;
  for (const entry of list.slice()) {
    if (entry.capture !== capturing) continue;
    if (!list.includes(entry)) continue;
    if (entry.once) removeEntry(target, event.type, entry);
    const handler =
      typeof entry.callback === 'function' ? entry.callback : entry.callback.handleEvent;
    if (typeof handler !== 'function') continue;
    try {
      handler.call(target, event);
    } catch (error) {
      console.error(error);
    }
    if (event[kStopImmediate]) return;
  }
};

const removeEntry = (target, type, entry) => {
  const list = listenersFor(target, type, false);
  if (!list) return;
  const at = list.indexOf(entry);
  if (at !== -1) list.splice(at, 1);
};

const eventTarget = {
  addEventListener(type, callback, options) {
    if (!callback) return;
    const capture = optionOf(options, 'capture');
    const list = listenersFor(this, String(type), true);
    // The identity of a listener is target, type, callback and capture
    // together: the same function may be registered once for each phase, and
    // registering it twice for one phase does nothing.
    if (!list.some((entry) => entry.callback === callback && entry.capture === capture)) {
      list.push({ callback, capture, once: optionOf(options, 'once') });
    }
  },

  removeEventListener(type, callback, options) {
    const capture = optionOf(options, 'capture');
    const list = listenersFor(this, String(type), false);
    if (!list) return;
    const at = list.findIndex(
      (entry) => entry.callback === callback && entry.capture === capture,
    );
    if (at !== -1) list.splice(at, 1);
  },

  // The three phases, over the path from the window down to the target and
  // back. This is the whole reason a page can put one listener on a container
  // and hear about every button in it, which is how every list, menu and grid
  // on the web is written.
  dispatchEvent(event) {
    if (!event || typeof event.type !== 'string') {
      throw new TypeError('dispatchEvent expects an Event');
    }
    if (event[kDispatch]) {
      throw new TypeError('the event is already being dispatched');
    }

    const path = propagationPath(this);
    event[kDispatch] = true;
    event[kStop] = false;
    event[kStopImmediate] = false;
    event[kTarget] = this;
    event.defaultPrevented = false;
    // The path as it was, for as long as this dispatch lasts: the engine's own
    // `composedPath` answers with the target alone, which is right for an
    // event target that is not in a tree and wrong for one that is.
    const composed = path.slice();
    const ownComposedPath = Object.getOwnPropertyDescriptor(event, 'composedPath');
    Object.defineProperty(event, 'composedPath', {
      value: () => (event[kDispatch] ? composed.slice() : []),
      writable: true,
      enumerable: false,
      configurable: true,
    });

    try {
      // Down: the window first, the target's parent last.
      event.eventPhase = CAPTURING_PHASE;
      for (let index = path.length - 1; index > 0; index--) {
        if (event[kStop]) break;
        fire(path[index], event, true);
      }

      // At the target both kinds run, capture listeners first, and neither
      // counts as capturing or bubbling.
      if (!event[kStop]) {
        event.eventPhase = AT_TARGET;
        fire(this, event, true);
        if (!event[kStopImmediate]) fire(this, event, false);
      }

      // Up, if the event says it does. One that does not is heard only where
      // it landed.
      if (event.bubbles) {
        event.eventPhase = BUBBLING_PHASE;
        for (let index = 1; index < path.length; index++) {
          if (event[kStop]) break;
          fire(path[index], event, false);
        }
      }
    } finally {
      event.eventPhase = 0;
      event.currentTarget = null;
      event[kDispatch] = false;
      if (ownComposedPath) {
        Object.defineProperty(event, 'composedPath', ownComposedPath);
      } else {
        delete event.composedPath;
      }
    }

    return !event.defaultPrevented;
  },
};

for (const target of [globalThis, Node.prototype]) {
  for (const name of ['addEventListener', 'removeEventListener', 'dispatchEvent']) {
    Object.defineProperty(target, name, {
      value: eventTarget[name],
      writable: true,
      enumerable: false,
      configurable: true,
    });
  }
}
