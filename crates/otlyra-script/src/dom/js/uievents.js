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
