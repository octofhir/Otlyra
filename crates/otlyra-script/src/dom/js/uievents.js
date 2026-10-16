// The UI Events interfaces (and the few HTML event interfaces pages construct)
// on top of the engine's `Event`. Each init dictionary member becomes a
// read-only attribute on the prototype, read from the instance's own record;
// the dictionaries inherit, so `new MouseEvent('click', {bubbles: true,
// clientX: 4})` sets the `Event`, `UIEvent` and `MouseEvent` parts alike.
//
// No `TouchEvent`: a page that finds one decides it is on a touch screen.

const kEventInit = Symbol('otlyra.event.init');

const eventInterface = (name, Parent, defaults, extra = {}) => {
  const Interface = class extends Parent {
    constructor(type, init) {
      if (arguments.length === 0) {
        throw new TypeError(`Failed to construct '${name}': 1 argument required`);
      }
      super(type, init);
      const source = init !== null && typeof init === 'object' ? init : {};
      const record = this[kEventInit] || {};
      for (const key of Object.keys(defaults)) {
        record[key] = source[key] === undefined ? defaults[key] : source[key];
      }
      Object.defineProperty(this, kEventInit, {
        value: record,
        writable: true,
        enumerable: false,
        configurable: true,
      });
    }
  };
  Object.defineProperty(Interface, 'name', { value: name });
  Object.defineProperty(Interface.prototype, Symbol.toStringTag, {
    value: name,
    configurable: true,
  });
  for (const key of Object.keys(defaults)) {
    accessor(Interface.prototype, key, function () {
      const record = this[kEventInit];
      return record && key in record ? record[key] : defaults[key];
    });
  }
  for (const key of Object.keys(extra)) {
    method(Interface.prototype, key, extra[key]);
  }
  defineGlobal(name, Interface);
  return Interface;
};

const modifierState = function getModifierState(key) {
  switch (String(key)) {
    case 'Control':
      return Boolean(this.ctrlKey);
    case 'Shift':
      return Boolean(this.shiftKey);
    case 'Alt':
      return Boolean(this.altKey);
    case 'Meta':
      return Boolean(this.metaKey);
    default:
      return false;
  }
};

const UI_DEFAULTS = { view: null, detail: 0, which: 0 };
const MODIFIER_DEFAULTS = {
  ctrlKey: false,
  shiftKey: false,
  altKey: false,
  metaKey: false,
};
const MOUSE_DEFAULTS = {
  ...MODIFIER_DEFAULTS,
  screenX: 0,
  screenY: 0,
  clientX: 0,
  clientY: 0,
  movementX: 0,
  movementY: 0,
  button: 0,
  buttons: 0,
  relatedTarget: null,
};

const UIEventInterface = eventInterface('UIEvent', Event, UI_DEFAULTS);
const MouseEventInterface = eventInterface(
  'MouseEvent',
  UIEventInterface,
  { ...UI_DEFAULTS, ...MOUSE_DEFAULTS },
  { getModifierState: modifierState },
);
// CSSOM View's aliases and the page/offset coordinates. Scrolling is the
// document's; offsets are relative to the target's border box.
for (const [alias, key] of [['x', 'clientX'], ['y', 'clientY']]) {
  accessor(MouseEventInterface.prototype, alias, function () {
    return this[key];
  });
}
accessor(MouseEventInterface.prototype, 'pageX', function () {
  return this.clientX + (globalThis.scrollX || 0);
});
accessor(MouseEventInterface.prototype, 'pageY', function () {
  return this.clientY + (globalThis.scrollY || 0);
});
const offsetFrom = (event, axis) => {
  const target = event.target;
  if (!target || typeof target.getBoundingClientRect !== 'function') {
    return axis === 'x' ? event.clientX : event.clientY;
  }
  const rect = target.getBoundingClientRect();
  return axis === 'x' ? event.clientX - rect.left : event.clientY - rect.top;
};
accessor(MouseEventInterface.prototype, 'offsetX', function () {
  return offsetFrom(this, 'x');
});
accessor(MouseEventInterface.prototype, 'offsetY', function () {
  return offsetFrom(this, 'y');
});

eventInterface('PointerEvent', MouseEventInterface, {
  ...UI_DEFAULTS,
  ...MOUSE_DEFAULTS,
  pointerId: 0,
  width: 1,
  height: 1,
  pressure: 0,
  tangentialPressure: 0,
  tiltX: 0,
  tiltY: 0,
  twist: 0,
  altitudeAngle: Math.PI / 2,
  azimuthAngle: 0,
  pointerType: '',
  isPrimary: false,
});

const WheelEventInterface = eventInterface('WheelEvent', MouseEventInterface, {
  ...UI_DEFAULTS,
  ...MOUSE_DEFAULTS,
  deltaX: 0,
  deltaY: 0,
  deltaZ: 0,
  deltaMode: 0,
});
for (const [name, value] of [['DOM_DELTA_PIXEL', 0], ['DOM_DELTA_LINE', 1], ['DOM_DELTA_PAGE', 2]]) {
  const constant = { value, enumerable: true };
  Object.defineProperty(WheelEventInterface, name, constant);
  Object.defineProperty(WheelEventInterface.prototype, name, constant);
}

const KeyboardEventInterface = eventInterface(
  'KeyboardEvent',
  UIEventInterface,
  {
    ...UI_DEFAULTS,
    ...MODIFIER_DEFAULTS,
    key: '',
    code: '',
    location: 0,
    repeat: false,
    isComposing: false,
    charCode: 0,
    keyCode: 0,
  },
  { getModifierState: modifierState },
);
for (const [name, value] of [
  ['DOM_KEY_LOCATION_STANDARD', 0],
  ['DOM_KEY_LOCATION_LEFT', 1],
  ['DOM_KEY_LOCATION_RIGHT', 2],
  ['DOM_KEY_LOCATION_NUMPAD', 3],
]) {
  const constant = { value, enumerable: true };
  Object.defineProperty(KeyboardEventInterface, name, constant);
  Object.defineProperty(KeyboardEventInterface.prototype, name, constant);
}

eventInterface('FocusEvent', UIEventInterface, { ...UI_DEFAULTS, relatedTarget: null });
eventInterface('InputEvent', UIEventInterface, {
  ...UI_DEFAULTS,
  data: null,
  inputType: '',
  isComposing: false,
  dataTransfer: null,
});
eventInterface('CompositionEvent', UIEventInterface, { ...UI_DEFAULTS, data: '' });
eventInterface('SubmitEvent', Event, { submitter: null });
eventInterface('HashChangeEvent', Event, { oldURL: '', newURL: '' });
eventInterface('PopStateEvent', Event, { state: null });
eventInterface('PageTransitionEvent', Event, { persisted: false });
eventInterface('AnimationEvent', Event, { animationName: '', elapsedTime: 0, pseudoElement: '' });
eventInterface('TransitionEvent', Event, { propertyName: '', elapsedTime: 0, pseudoElement: '' });

// The legacy way to make an event (DOM §4.5 `createEvent`, and the `init*`
// methods): older libraries and polyfills still build events like this, and
// YouTube's web components polyfill tests `initEvent` before anything else.
const setInit = (event, values) => {
  const record = event[kEventInit] || {};
  Object.assign(record, values);
  Object.defineProperty(event, kEventInit, {
    value: record,
    writable: true,
    enumerable: false,
    configurable: true,
  });
};
method(Event.prototype, 'initEvent', function initEvent(type, bubbles, cancelable) {
  this.type = String(type);
  this.bubbles = Boolean(bubbles);
  this.cancelable = Boolean(cancelable);
  this.defaultPrevented = false;
});
method(CustomEvent.prototype, 'initCustomEvent', function initCustomEvent(
  type,
  bubbles,
  cancelable,
  detail,
) {
  this.initEvent(type, bubbles, cancelable);
  Object.defineProperty(this, 'detail', {
    value: detail === undefined ? null : detail,
    writable: false,
    enumerable: true,
    configurable: true,
  });
});
method(UIEventInterface.prototype, 'initUIEvent', function initUIEvent(
  type,
  bubbles,
  cancelable,
  view,
  detail,
) {
  this.initEvent(type, bubbles, cancelable);
  setInit(this, { view: view === undefined ? null : view, detail: Number(detail) || 0 });
});
method(MouseEventInterface.prototype, 'initMouseEvent', function initMouseEvent(
  type,
  bubbles,
  cancelable,
  view,
  detail,
  screenX,
  screenY,
  clientX,
  clientY,
  ctrlKey,
  altKey,
  shiftKey,
  metaKey,
  button,
  relatedTarget,
) {
  this.initUIEvent(type, bubbles, cancelable, view, detail);
  setInit(this, {
    screenX: Number(screenX) || 0,
    screenY: Number(screenY) || 0,
    clientX: Number(clientX) || 0,
    clientY: Number(clientY) || 0,
    ctrlKey: Boolean(ctrlKey),
    altKey: Boolean(altKey),
    shiftKey: Boolean(shiftKey),
    metaKey: Boolean(metaKey),
    button: Number(button) || 0,
    relatedTarget: relatedTarget === undefined ? null : relatedTarget,
  });
});
method(KeyboardEventInterface.prototype, 'initKeyboardEvent', function initKeyboardEvent(
  type,
  bubbles,
  cancelable,
  view,
  key,
  location,
  ctrlKey,
  altKey,
  shiftKey,
  metaKey,
) {
  this.initUIEvent(type, bubbles, cancelable, view, 0);
  setInit(this, {
    key: key === undefined ? '' : String(key),
    location: Number(location) || 0,
    ctrlKey: Boolean(ctrlKey),
    altKey: Boolean(altKey),
    shiftKey: Boolean(shiftKey),
    metaKey: Boolean(metaKey),
  });
});

// The names `createEvent` knows (DOM §4.5), lowercased. `TouchEvent` is
// refused on purpose, which is how pages ask whether the screen is touched.
const LEGACY_EVENT_INTERFACES = {
  beforeunloadevent: 'Event',
  compositionevent: 'CompositionEvent',
  customevent: 'CustomEvent',
  event: 'Event',
  events: 'Event',
  focusevent: 'FocusEvent',
  hashchangeevent: 'HashChangeEvent',
  htmlevents: 'Event',
  keyboardevent: 'KeyboardEvent',
  mouseevent: 'MouseEvent',
  mouseevents: 'MouseEvent',
  svgevents: 'Event',
  uievent: 'UIEvent',
  uievents: 'UIEvent',
};
method(Document.prototype, 'createEvent', function createEvent(name) {
  const interfaceName = LEGACY_EVENT_INTERFACES[String(name).toLowerCase()];
  const Interface = interfaceName && globalThis[interfaceName];
  if (typeof Interface !== 'function') {
    throw new DOMException(
      "The provided event type ('" + String(name) + "') is invalid.",
      'NotSupportedError',
    );
  }
  return new Interface('');
});
