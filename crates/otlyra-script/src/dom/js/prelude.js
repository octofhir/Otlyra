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

// The tree's mutation methods as the glue leaves them, custom element
// reactions and script insertion included, captured by `seal.js` once every
// file has wrapped them. The glue's own DOM methods change the tree through
// these and never through what a page has since put on `Node.prototype`:
// `el.after(x)` in a browser does not call a page's `insertBefore`, and a
// consent manager that patches it would hear its own half-built hook.
const treeOps = {};
const callTree = (name, target, ...args) => treeOps[name].apply(target, args);

// An attribute this engine has no state behind: a getter that always answers
// `value`, as WebIDL makes even a read-only attribute an accessor — code that
// wraps `Object.getOwnPropertyDescriptor(X.prototype, name).get` finds one.
const readonly = (object, name, value) => {
  accessor(object, name, { [name]() { return value; } }[name], undefined);
};

// A writable attribute with nothing behind it but what was last written:
// `initial` until then, per object.
const stored = (object, name, initial) => {
  const values = new WeakMap();
  accessor(
    object,
    name,
    { [name]() { return values.has(this) ? values.get(this) : initial; } }[name],
    { [name](value) { values.set(this, value); } }[name],
  );
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
// Documents script makes beside the page's (see compat.js).
const nativeCreateDocument = take(Document, '__create');
const nativeSecondaryType = take(Document.prototype, '__contentType');
const nativeParseInto = take(Document.prototype, '__parse');
const nativeCreateDoctype = take(Document.prototype, '__createDoctype');
// Whether `document` is one script made rather than the page's.
const isMadeDocument = (document) => nativeSecondaryType.call(document) !== null;
const nativeNavigate = take(Document, '__navigate');
const nativeReload = take(Document, '__reload');
const nativeFrameRequested = take(Document, '__frameRequested');
const nativePointerMovesWanted = take(Document, '__pointerMovesWanted');
const nativeScrollTo = take(Document, '__scrollTo');
const nativeGeometryWanted = take(Document, '__geometryWanted');
const nativeScrollEventsWanted = take(Document, '__scrollEventsWanted');
const nativeWheelEventsWanted = take(Document, '__wheelEventsWanted');
const nativeAttributeRecords = take(Element.prototype, '__attributeRecords');
// Whether a node is in the page's tree: what running a script, connecting a
// custom element or drawing a picture asks. `isConnected` is also true in a
// document script made, where none of those happen.
const nativeInPage = take(Node.prototype, '__inPage');
const inPage = (node) => nativeInPage.call(node);
// Whether a node belongs to the page's document rather than one script made,
// whose pictures are never fetched and whose scripts never run.
const nativeOwnerDocument = Object.getOwnPropertyDescriptor(Node.prototype, 'ownerDocument').get;
const ofPage = (node) => {
  const page = nativeDocument();
  return node === page || nativeOwnerDocument.call(node) === page;
};
