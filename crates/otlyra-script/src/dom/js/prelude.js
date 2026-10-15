// The half of the DOM that is better written in JavaScript than generated.
//
// The native classes — Node, Element, CharacterData, Text, Comment, Document,
// DocumentFragment, DocumentType — are Rust, and hold nothing but a node's id.
// What these files add is what sits on top of them: the interface tree every
// element is made as, the list types, the mixins whose arguments are
// variadic, attribute reflection, the window, and listener registration that
// keeps its callbacks where the collector can see them.
//
// The files are one script. `build.rs` joins them, in order, inside one
// function, after a generated `INTERFACE_TREE`: what one declares the next can
// use, and nothing leaks to the page except what is defined on the global.

const defineGlobal = (name, value) => {
  Object.defineProperty(globalThis, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
};

// Fill in a member the engine did not provide, and leave one it did.
const define = (object, name, value) => {
  if (!object || name in object) return;
  Object.defineProperty(object, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
};

// A method, replacing whatever was there.
const method = (object, name, value) => {
  Object.defineProperty(object, name, {
    value,
    writable: true,
    enumerable: false,
    configurable: true,
  });
};

// An accessor, replacing whatever was there. Attributes are enumerable on
// their prototypes, as WebIDL makes them.
const accessor = (object, name, get, set) => {
  Object.defineProperty(object, name, {
    get,
    set,
    enumerable: true,
    configurable: true,
  });
};

// The object in `object`'s prototype chain that owns `name`. A native member
// lives on the native class's prototype, which is not always the prototype of
// the global by that name: `Text`, `Comment` and `DocumentFragment` are
// subclasses of their native classes — see `interfaces.js`.
const owner = (object, name) => {
  for (let current = object; current; current = Object.getPrototypeOf(current)) {
    if (Object.prototype.hasOwnProperty.call(current, name)) return current;
  }
  throw new TypeError('the native member ' + name + ' is missing');
};

// Take a native member off its prototype for the glue's own use: the getter of
// an accessor, the function of a method.
const take = (object, name) => {
  const holder = owner(object, name);
  const descriptor = Object.getOwnPropertyDescriptor(holder, name);
  delete holder[name];
  return descriptor.get || descriptor.value;
};

// The natives report a DOMException as a TypeError whose message starts with
// the exception's name — the engine has no native DOMException class yet — and
// this turns it back into the real thing, so a page that catches by name or by
// class catches what it expects.
const DOM_ERROR = /^([A-Z][A-Za-z]*Error): ([^]*)$/;

const domException = (error) => {
  if (error instanceof TypeError && typeof DOMException === 'function') {
    const match = DOM_ERROR.exec(error.message);
    if (match) return new DOMException(match[2], match[1]);
  }
  return error;
};

// Wrap `object[name]` so the exceptions it throws are the platform's.
const throwsDomExceptions = (object, name) => {
  const holder = owner(object, name);
  const native = holder[name];
  const wrapped = {
    [name](...args) {
      try {
        return native.apply(this, args);
      } catch (error) {
        throw domException(error);
      }
    },
  }[name];
  Object.defineProperty(wrapped, 'length', { value: native.length });
  method(holder, name, wrapped);
};

// The members the natives keep for the glue, taken now so a page never sees
// them.
const nativeDocument = take(Document, '__self');
const nativeUrl = take(Document, '__url');
const nativeNavigate = take(Document, '__navigate');
const nativeReload = take(Document, '__reload');
const nativeFrameRequested = take(Document, '__frameRequested');
const nativePointerMovesWanted = take(Document, '__pointerMovesWanted');
const nativeAttributeRecords = take(Element.prototype, '__attributeRecords');
