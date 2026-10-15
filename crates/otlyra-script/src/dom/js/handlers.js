// Event handlers (HTML §8.1.8): `button.onclick = f`, `window.onload = f`
// and `<a onclick="...">`. A handler is not a second listener list. It is one
// listener per target and type, registered when the handler is first given a
// value and kept in that place in the target's ordinary list; the value it
// calls can change without moving it, and setting it to null removes it.
//
// A content attribute is compiled lazily — when its IDL attribute is read or
// an event it handles reaches its element — which is also when a parser-made
// attribute first registers its listener. Parsed attributes exist before any
// script could add a listener, so that listener goes first.

const HTML_NAMESPACE = 'http://www.w3.org/1999/xhtml';

// GlobalEventHandlers, minus touch: a desktop that offers `ontouchstart` is a
// touch device to every page that feature-detects one.
const GLOBAL_HANDLER_TYPES = [
  'abort', 'animationcancel', 'animationend', 'animationiteration', 'animationstart',
  'auxclick', 'beforeinput', 'beforetoggle', 'blur', 'cancel', 'canplay',
  'canplaythrough', 'change', 'click', 'close', 'contextlost', 'contextmenu',
  'contextrestored', 'copy', 'cuechange', 'cut', 'dblclick', 'drag', 'dragend',
  'dragenter', 'dragleave', 'dragover', 'dragstart', 'drop', 'durationchange',
  'emptied', 'ended', 'error', 'focus', 'formdata', 'gotpointercapture', 'input',
  'invalid', 'keydown', 'keypress', 'keyup', 'load', 'loadeddata', 'loadedmetadata',
  'loadstart', 'lostpointercapture', 'mousedown', 'mouseenter', 'mouseleave',
  'mousemove', 'mouseout', 'mouseover', 'mouseup', 'paste', 'pause', 'play',
  'playing', 'pointercancel', 'pointerdown', 'pointerenter', 'pointerleave',
  'pointermove', 'pointerout', 'pointerover', 'pointerup', 'progress', 'ratechange',
  'reset', 'resize', 'scroll', 'scrollend', 'securitypolicyviolation', 'seeked',
  'seeking', 'select', 'selectionchange', 'selectstart', 'slotchange', 'stalled',
  'submit', 'suspend', 'timeupdate', 'toggle', 'transitioncancel', 'transitionend',
  'transitionrun', 'transitionstart', 'volumechange', 'waiting', 'wheel',
];

// WindowEventHandlers: on the window, and reflected by `<body>`/`<frameset>`.
const WINDOW_HANDLER_TYPES = [
  'afterprint', 'beforeprint', 'beforeunload', 'hashchange', 'languagechange',
  'message', 'messageerror', 'offline', 'online', 'pagehide', 'pageshow', 'popstate',
  'rejectionhandled', 'storage', 'unhandledrejection', 'unload',
];

// What a `<body>` attribute or IDL attribute sets on its window rather than on
// itself: the window handlers and these GlobalEventHandlers.
const WINDOW_REFLECTING_TYPES = new Set([
  ...WINDOW_HANDLER_TYPES, 'blur', 'error', 'focus', 'load', 'resize', 'scroll',
]);

// target -> type -> { value, attribute, entry }. `attribute` is the content
// attribute text last compiled, `undefined` before it was ever looked at.
const handlerStates = new Map();

const handlerStateFor = (target, type, create) => {
  let byType = handlerStates.get(target);
  if (!byType) {
    if (!create) return undefined;
    byType = new Map();
    handlerStates.set(target, byType);
  }
  let state = byType.get(type);
  if (!state && create) {
    state = { value: null, attribute: undefined, entry: null };
    byType.set(type, state);
  }
  return state;
};

const isWindowReflector = (element) =>
  element !== null &&
  typeof element === 'object' &&
  element.nodeType === 1 &&
  element.namespaceURI === HTML_NAMESPACE &&
  (element.localName === 'body' || element.localName === 'frameset');

// The element whose `on…` attribute names this target's handler, if any.
const attributeHost = (target, type) => {
  if (target === globalThis) {
    if (!WINDOW_REFLECTING_TYPES.has(type)) return null;
    const body = document.body;
    return isWindowReflector(body) ? body : null;
  }
  if (target === null || typeof target !== 'object' || target.nodeType !== 1) return null;
  // A body's window-reflecting attribute belongs to the window.
  if (isWindowReflector(target) && WINDOW_REFLECTING_TYPES.has(type)) return null;
  return target;
};

// `with` makes the element, its form and its document the handler's scope, as
// the specification's scope chain does. An engine without `with` gets a plain
// function: `this` and globals still work, bare element members do not.
const scopeSupported = (() => {
  try {
    new Function('with ({}) {}');
    return true;
  } catch {
    return false;
  }
})();

const compileHandler = (element, type, body, forWindow) => {
  const parameters =
    forWindow && type === 'error' ? ['event', 'source', 'lineno', 'colno', 'error'] : ['event'];
  try {
    if (!scopeSupported) return new Function(...parameters, body);
    const make = new Function(
      '__otlyraScopeDocument',
      '__otlyraScopeForm',
      '__otlyraScopeElement',
      `return function (${parameters.join(', ')}) {\n` +
        'with (__otlyraScopeDocument) { with (__otlyraScopeForm) { with (__otlyraScopeElement) {\n' +
        `${body}\n}}}};`,
    );
    const form = !forWindow && element.form && typeof element.form === 'object' ? element.form : {};
    return make(element.ownerDocument || document, form, forWindow ? {} : element);
  } catch (error) {
    // An attribute that does not compile is reported and handles nothing.
    console.error(error);
    return null;
  }
};

const handlerEntry = (target, type) => ({
  capture: false,
  once: false,
  callback(event) {
    return invokeHandler(target, type, event);
  },
});

const activateHandler = (target, type, state, first) => {
  if (state.entry) return;
  state.entry = handlerEntry(target, type);
  const list = listenersFor(target, type, true);
  if (first) list.unshift(state.entry);
  else list.push(state.entry);
};

const deactivateHandler = (target, type, state) => {
  state.value = null;
  if (state.entry) {
    removeEntry(target, type, state.entry);
    state.entry = null;
  }
};

// Bring the handler up to date with its content attribute: compile one that
// appeared or changed, and drop one whose attribute was removed.
const syncAttributeHandler = (target, type) => {
  const host = attributeHost(target, type);
  if (host === null) return;
  const text = host.getAttribute(`on${type}`);
  const state = handlerStateFor(target, type, text !== null);
  if (!state || state.attribute === text) return;
  const before = state.attribute;
  state.attribute = text;
  if (text === null) {
    if (before !== undefined) deactivateHandler(target, type, state);
    return;
  }
  state.value = compileHandler(host, type, text, target === globalThis);
  if (state.value === null) {
    deactivateHandler(target, type, state);
  } else {
    activateHandler(target, type, state, before === undefined);
  }
};

const invokeHandler = (target, type, event) => {
  const state = handlerStateFor(target, type, false);
  const value = state ? state.value : null;
  if (value === null || value === undefined) return;
  if (typeof value !== 'function') {
    throw new TypeError(`the on${type} handler is not callable`);
  }
  if (
    type === 'error' &&
    target === globalThis &&
    typeof ErrorEvent === 'function' &&
    event instanceof ErrorEvent
  ) {
    const result = value.call(target, event.message, event.filename, event.lineno, event.colno, event.error);
    if (result === true) event.preventDefault();
    return;
  }
  const result = value.call(target, event);
  if (type === 'beforeunload') {
    if (result !== undefined && result !== null) {
      event.preventDefault();
      if ('returnValue' in event) event.returnValue = String(result);
    }
    return;
  }
  if (result === false) event.preventDefault();
};

const getHandler = (target, type) => {
  syncAttributeHandler(target, type);
  const state = handlerStateFor(target, type, false);
  return state ? state.value : null;
};

const setHandler = (target, type, value) => {
  // The attribute as it stands is seen first, so the value set here is not
  // overwritten by the next event finding an attribute it never compiled.
  syncAttributeHandler(target, type);
  const next =
    value !== null && (typeof value === 'object' || typeof value === 'function') ? value : null;
  const state = handlerStateFor(target, type, next !== null);
  if (!state) return;
  if (next === null) {
    deactivateHandler(target, type, state);
    return;
  }
  state.value = next;
  activateHandler(target, type, state, false);
};

const defineHandler = (object, type, resolve = (target) => target) => {
  accessor(
    object,
    `on${type}`,
    function () {
      return getHandler(resolve(this), type);
    },
    function (value) {
      setHandler(resolve(this), type, value);
    },
  );
};

for (const type of GLOBAL_HANDLER_TYPES) {
  defineHandler(HTMLElement.prototype, type);
  if (typeof SVGElement === 'function') defineHandler(SVGElement.prototype, type);
  defineHandler(Document.prototype, type);
  defineHandler(globalThis, type, () => globalThis);
}
for (const type of WINDOW_HANDLER_TYPES) {
  defineHandler(globalThis, type, () => globalThis);
}
for (const type of ['readystatechange', 'visibilitychange']) {
  defineHandler(Document.prototype, type);
}
// `document.body.onload` is the window's.
for (const Interface of [globalThis.HTMLBodyElement, globalThis.HTMLFrameSetElement]) {
  if (typeof Interface !== 'function') continue;
  for (const type of WINDOW_REFLECTING_TYPES) {
    defineHandler(Interface.prototype, type, () => globalThis);
  }
}
