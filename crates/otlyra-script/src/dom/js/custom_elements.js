// Custom elements (HTML §4.13): the `customElements` registry, the upgrade of
// elements that were there before their definition, and the reactions —
// connected, disconnected, attribute changed — to what script does to them.
//
// All of it is here, over the natives. A custom element is a node like any
// other; what makes it custom is its wrapper's prototype, which an upgrade
// swaps for its definition's. Wrappers live as long as the page (see
// `identity.rs`), so the swap sticks to the node.
//
// What is not here: shadow trees, customized built-in elements (`is=`, which
// Safari does not implement either; their definitions are kept but nothing is
// upgraded to them) and form-associated elements.

const nativeNodeCount = take(Document, '__nodeCount');
const nativeCustomElementsDefined = take(Document, '__customElementsDefined');
const nativeIsCustomElementName = take(Document, '__isCustomElementName');
const nativeSetCustomized = take(Element.prototype, '__setCustomized');

const rawCreateElement = Document.prototype.createElement;
const rawGetAttribute = Element.prototype.getAttribute;

// The definitions, by name and by constructor.
const definitions = new Map();
const definitionsByConstructor = new Map();
// `whenDefined` promises for names not defined yet.
const awaitingDefinition = new Map();
// Each element's custom element state: 'custom' once upgraded, 'failed' when
// its constructor threw. Absent is "undefined", the state of an element
// whose definition has not run on it.
const customState = new WeakMap();
// Whether `connectedCallback` was the last of the two an element was given.
const reportedConnected = new WeakMap();
// The marker a construction stack holds once its element has been
// constructed (HTML §4.13.6).
const ALREADY_CONSTRUCTED = Object.freeze({});
// A selector matching every defined name, or '' while there is none.
let definedSelector = '';
// The document's node count when the checkpoint last looked.
let checkpointNodes = -1;

// An element a definition has run on: 'custom', and `:defined` from now on.
const markCustom = (element) => {
  customState.set(element, 'custom');
  nativeSetCustomized.call(element);
};

const escapeName = (name) => name.replace(/[^a-zA-Z0-9_-]/g, (character) => '\\' + character);

const report = (error) => {
  const message =
    error && typeof error === 'object' && 'message' in error
      ? 'Uncaught ' + (error.name || 'Error') + ': ' + error.message
      : 'Uncaught ' + String(error);
  try {
    __otlyraReportError(message, nativeUrl(), error);
  } catch (_) {
    console.error(error);
  }
};

const invoke = (element, definition, name, args) => {
  const callback = definition.callbacks[name];
  if (typeof callback !== 'function') return;
  try {
    callback.apply(element, args);
  } catch (error) {
    report(error);
  }
};

const definitionOf = (element) =>
  customState.get(element) === 'custom' ? definitions.get(element.localName) : undefined;

const connect = (element, definition) => {
  if (reportedConnected.get(element) === true) return;
  reportedConnected.set(element, true);
  invoke(element, definition, 'connectedCallback', []);
};

const disconnect = (element, definition) => {
  if (reportedConnected.get(element) !== true) return;
  reportedConnected.set(element, false);
  invoke(element, definition, 'disconnectedCallback', []);
};

// HTML §4.13.5 "upgrade an element".
const upgrade = (element, definition) => {
  if (!definition || definition.localName !== definition.name) return;
  if (customState.has(element)) return;
  // Failed until the constructor returns (step 3): an element whose
  // constructor reaches the checkpoint — by connecting something, say — is
  // not upgraded a second time from inside its own upgrade.
  customState.set(element, 'failed');
  definition.stack.push(element);
  let failure = null;
  try {
    const constructed = Reflect.construct(definition.constructor, []);
    if (constructed !== element) {
      throw new TypeError(
        'the constructor of <' + definition.name + '> did not return the element being upgraded',
      );
    }
  } catch (error) {
    failure = error;
  }
  definition.stack.pop();
  if (failure !== null) {
    customState.set(element, 'failed');
    report(failure);
    return;
  }
  markCustom(element);
  for (const attribute of definition.observed) {
    const value = rawGetAttribute.call(element, attribute);
    if (value !== null) {
      invoke(element, definition, 'attributeChangedCallback', [attribute, null, value, null]);
    }
  }
  if (inPage(element)) connect(element, definition);
};

// The elements script attached shadow roots to, so the walks below can go
// into their shadow trees, which no selector reaches.
const shadowHosts = new Set();

// The elements in `node`'s inclusive subtree whose names are defined, in
// tree order, and those in the shadow trees of hosts inside it.
const definedInside = (node) => {
  if (!definedSelector || !node) return [];
  const found = [];
  const type = node.nodeType;
  if (type === 1 && definitions.has(node.localName)) found.push(node);
  if (type === 1 || type === 9 || type === 11) {
    for (const element of Array.from(node.querySelectorAll(definedSelector))) found.push(element);
  }
  for (const host of shadowHosts) {
    if (host === node || node.contains(host)) {
      const root = nativeShadowRoot.call(host);
      if (root && root !== node) found.push(...definedInside(root));
    }
  }
  return found;
};

// What inserting `roots` asks of their custom elements (DOM §4.2.3 "insert",
// step 7.7): a connected one that was moved is disconnected and connected
// again, a newly connected one is connected, and an undefined one that is now
// connected is upgraded.
const settleInserted = (roots, wereConnected) => {
  roots.forEach((root, index) => {
    for (const element of definedInside(root)) {
      const state = customState.get(element);
      if (state === undefined) {
        if (inPage(element)) upgrade(element, definitions.get(element.localName));
        continue;
      }
      const definition = definitionOf(element);
      if (!definition) continue;
      if (wereConnected[index]) disconnect(element, definition);
      if (inPage(element)) connect(element, definition);
    }
  });
};

// What removing `root` asks of its custom elements (DOM §4.2.3 "remove").
const settleRemoved = (elements) => {
  for (const element of elements) {
    const definition = definitionOf(element);
    if (definition && !inPage(element)) disconnect(element, definition);
  }
};

const customInside = (node) => definedInside(node).filter((element) => definitionOf(element));

// The tree mutations, wrapped so their reactions run before they return, as
// `[CEReactions]` has them. Nothing extra is done for a page that defined
// nothing.
const wrapNodeMethod = (holder, name, around) => {
  const native = holder[name];
  const wrapped = {
    [name](...args) {
      if (!definedSelector) return native.apply(this, args);
      return around.call(this, native, args);
    },
  }[name];
  Object.defineProperty(wrapped, 'length', { value: native.length });
  method(holder, name, wrapped);
};

const insertedRoots = (node) =>
  node && node.nodeType === 11 ? Array.from(node.childNodes) : [node];

for (const name of ['appendChild', 'insertBefore']) {
  wrapNodeMethod(Node.prototype, name, function (native, args) {
    const roots = insertedRoots(args[0]);
    const were = roots.map((root) => Boolean(root && inPage(root)));
    const result = native.apply(this, args);
    settleInserted(roots, were);
    return result;
  });
}

wrapNodeMethod(Node.prototype, 'replaceChild', function (native, args) {
  const roots = insertedRoots(args[0]);
  const were = roots.map((root) => Boolean(root && inPage(root)));
  const leaving = customInside(args[1]);
  const result = native.apply(this, args);
  settleRemoved(leaving);
  settleInserted(roots, were);
  return result;
});

wrapNodeMethod(Node.prototype, 'removeChild', function (native, args) {
  const leaving = customInside(args[0]);
  const result = native.apply(this, args);
  settleRemoved(leaving);
  return result;
});

wrapNodeMethod(Element.prototype, 'remove', function (native, args) {
  const leaving = customInside(this);
  const result = native.apply(this, args);
  settleRemoved(leaving);
  return result;
});

// A copy is made undefined and upgraded at once, which is what creating an
// element with the synchronous flag unset comes to when nothing is in the
// way (DOM §4.5 "clone").
const upgradeInside = (root) => {
  // A document script made has no registry: nothing in it is custom.
  if (!ofPage(root)) return;
  for (const element of definedInside(root)) {
    if (!customState.has(element)) upgrade(element, definitions.get(element.localName));
  }
};
wrapNodeMethod(Node.prototype, 'cloneNode', function (native, args) {
  const copy = native.apply(this, args);
  upgradeInside(copy);
  return copy;
});
wrapNodeMethod(Document.prototype, 'importNode', function (native, args) {
  const copy = native.apply(this, args);
  upgradeInside(copy);
  return copy;
});

// Markup replaces a subtree wholesale: what was in it is disconnected and
// what the markup made is connected.
for (const name of ['innerHTML', 'outerHTML']) {
  const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, name);
  accessor(Element.prototype, name, descriptor.get, function (value) {
    if (!definedSelector) {
      descriptor.set.call(this, value);
      return;
    }
    const leaving = name === 'innerHTML'
      ? Array.from(this.childNodes).flatMap(customInside)
      : customInside(this);
    const parent = name === 'innerHTML' ? this : this.parentNode;
    const before = name === 'innerHTML' ? null : this.previousSibling;
    const after = name === 'innerHTML' ? null : this.nextSibling;
    descriptor.set.call(this, value);
    settleRemoved(leaving);
    if (!parent) return;
    const arrived = [];
    let node = before ? before.nextSibling : parent.firstChild;
    while (node && node !== after) {
      arrived.push(node);
      node = node.nextSibling;
    }
    settleInserted(arrived, arrived.map(() => false));
  });
}
wrapNodeMethod(Element.prototype, 'insertAdjacentHTML', function (native, args) {
  const result = native.apply(this, args);
  checkpoint(true);
  return result;
});

// `createElement` with a defined name runs the constructor there and then
// (DOM §4.5 "create an element", synchronous custom elements flag set).
const createDefined = (document, definition) => {
  try {
    const element = Reflect.construct(definition.constructor, []);
    if (
      !(element instanceof HTMLElement) ||
      element.hasAttributes() ||
      element.hasChildNodes() ||
      element.parentNode ||
      element.localName !== definition.localName
    ) {
      throw new DOMException(
        'the constructor of <' + definition.name + '> did not make a new element of its own',
        'NotSupportedError',
      );
    }
    return element;
  } catch (error) {
    report(error);
    const element = rawCreateElement.call(document, definition.localName);
    customState.set(element, 'failed');
    return element;
  }
};
wrapNodeMethod(Document.prototype, 'createElement', function (native, args) {
  if (isMadeDocument(this)) return native.apply(this, args);
  const definition = definitions.get(String(args[0]).toLowerCase());
  if (!definition || definition.localName !== definition.name) return native.apply(this, args);
  return createDefined(this, definition);
});
wrapNodeMethod(Document.prototype, 'createElementNS', function (native, args) {
  if (isMadeDocument(this)) return native.apply(this, args);
  const definition =
    args[0] === 'http://www.w3.org/1999/xhtml' ? definitions.get(String(args[1])) : undefined;
  if (!definition || definition.localName !== definition.name) return native.apply(this, args);
  return createDefined(this, definition);
});

// HTML §3.2.3, the HTML element constructor: a custom element's class
// constructed from script makes a new element; one constructed by an upgrade
// takes the element waiting on its construction stack.
htmlElementConstructor = (newTarget) => {
  const definition = definitionsByConstructor.get(newTarget);
  if (!definition || definition.localName !== definition.name) {
    throw new TypeError('Illegal constructor');
  }
  const prototype = newTarget.prototype;
  if (definition.stack.length === 0) {
    const element = rawCreateElement.call(document, definition.localName);
    Object.setPrototypeOf(element, prototype);
    markCustom(element);
    return element;
  }
  const element = definition.stack[definition.stack.length - 1];
  if (element === ALREADY_CONSTRUCTED) {
    throw new DOMException(
      'the constructor of <' + definition.name + '> was called twice for one element',
      'InvalidStateError',
    );
  }
  Object.setPrototypeOf(element, prototype);
  definition.stack[definition.stack.length - 1] = ALREADY_CONSTRUCTED;
  return element;
};

// Attribute changes, for the attributes a definition observes.
const observing = (element, name) => {
  const definition = definitionOf(element);
  return definition && definition.observedSet.has(name) ? definition : undefined;
};
const htmlName = (element, name) =>
  element.namespaceURI === 'http://www.w3.org/1999/xhtml' ? String(name).toLowerCase() : String(name);

const changeAttribute = (element, name, change) => {
  const definition = observing(element, name);
  if (!definition) return change();
  const old = rawGetAttribute.call(element, name);
  const result = change();
  const value = rawGetAttribute.call(element, name);
  if (old !== null || value !== null) {
    invoke(element, definition, 'attributeChangedCallback', [name, old, value, null]);
  }
  return result;
};

for (const name of ['setAttribute', 'removeAttribute', 'toggleAttribute']) {
  wrapNodeMethod(Element.prototype, name, function (native, args) {
    return changeAttribute(this, htmlName(this, args[0]), () => native.apply(this, args));
  });
}
wrapNodeMethod(Element.prototype, 'setAttributeNS', function (native, args) {
  if (args[0] !== null && args[0] !== undefined && args[0] !== '') return native.apply(this, args);
  const local = String(args[1]).split(':').pop();
  return changeAttribute(this, local, () => native.apply(this, args));
});
wrapNodeMethod(Element.prototype, 'removeAttributeNS', function (native, args) {
  if (args[0] !== null && args[0] !== undefined && args[0] !== '') return native.apply(this, args);
  return changeAttribute(this, String(args[1]), () => native.apply(this, args));
});
for (const [property, attribute] of [
  ['id', 'id'],
  ['className', 'class'],
]) {
  const holder = owner(Element.prototype, property);
  const descriptor = Object.getOwnPropertyDescriptor(holder, property);
  accessor(holder, property, descriptor.get, function (value) {
    if (!definedSelector) return descriptor.set.call(this, value);
    return changeAttribute(this, attribute, () => descriptor.set.call(this, value));
  });
}

// The elements the parser made since script last ran, upgraded before it
// runs again; the host calls this at the start of each task once the page
// has defined something. `force` looks even when no node is new, for markup
// inserted in a way the wrappers above do not follow.
const checkpoint = (force) => {
  if (!definedSelector) return;
  const count = nativeNodeCount();
  if (!force && count === checkpointNodes) return;
  checkpointNodes = count;
  for (const element of Array.from(document.querySelectorAll(definedSelector))) {
    const state = customState.get(element);
    if (state === undefined) {
      upgrade(element, definitions.get(element.localName));
    } else if (state === 'custom') {
      connect(element, definitions.get(element.localName));
    }
  }
};
defineGlobal('__otlyraCustomElementsCheckpoint', function __otlyraCustomElementsCheckpoint() {
  checkpoint(false);
});

const LIFECYCLE = [
  'connectedCallback',
  'disconnectedCallback',
  'adoptedCallback',
  'attributeChangedCallback',
];

class CustomElementRegistry {
  constructor() {
    throw new TypeError('Illegal constructor');
  }

  // HTML §4.13.4 `define`.
  define(name, constructor, options) {
    name = String(name);
    if (typeof constructor !== 'function') {
      throw new TypeError('a custom element definition needs a constructor');
    }
    if (!nativeIsCustomElementName(name)) {
      throw new DOMException("'" + name + "' is not a valid custom element name", 'SyntaxError');
    }
    if (definitions.has(name)) {
      throw new DOMException("'" + name + "' has already been defined", 'NotSupportedError');
    }
    if (definitionsByConstructor.has(constructor)) {
      throw new DOMException(
        'this constructor has already been used with another name',
        'NotSupportedError',
      );
    }
    let localName = name;
    const extending = options && options.extends != null ? String(options.extends) : null;
    if (extending !== null) {
      if (nativeIsCustomElementName(extending)) {
        throw new DOMException("'" + extending + "' is a custom element name", 'NotSupportedError');
      }
      localName = extending;
    }
    const prototype = constructor.prototype;
    if (prototype === null || (typeof prototype !== 'object' && typeof prototype !== 'function')) {
      throw new TypeError("a custom element constructor's prototype must be an object");
    }
    const callbacks = {};
    for (const callback of LIFECYCLE) {
      const value = prototype[callback];
      if (value !== undefined && typeof value !== 'function') {
        throw new TypeError(callback + ' must be a function');
      }
      callbacks[callback] = value;
    }
    let observed = [];
    if (typeof callbacks.attributeChangedCallback === 'function') {
      const attributes = constructor.observedAttributes;
      if (attributes !== undefined) observed = Array.from(attributes, String);
    }
    const definition = {
      name,
      localName,
      constructor,
      callbacks,
      observed,
      observedSet: new Set(observed),
      stack: [],
    };
    definitions.set(name, definition);
    definitionsByConstructor.set(constructor, definition);
    if (localName === name) {
      definedSelector += (definedSelector ? ',' : '') + escapeName(name);
      nativeCustomElementsDefined();
      for (const element of Array.from(document.querySelectorAll(escapeName(name)))) {
        upgrade(element, definition);
      }
    }
    const waiting = awaitingDefinition.get(name);
    if (waiting) {
      awaitingDefinition.delete(name);
      waiting.resolve(constructor);
    }
  }

  get(name) {
    const definition = definitions.get(String(name));
    return definition ? definition.constructor : undefined;
  }

  getName(constructor) {
    const definition = definitionsByConstructor.get(constructor);
    return definition ? definition.name : null;
  }

  whenDefined(name) {
    name = String(name);
    if (!nativeIsCustomElementName(name)) {
      return Promise.reject(
        new DOMException("'" + name + "' is not a valid custom element name", 'SyntaxError'),
      );
    }
    const definition = definitions.get(name);
    if (definition) return Promise.resolve(definition.constructor);
    let waiting = awaitingDefinition.get(name);
    if (!waiting) {
      let resolve;
      const promise = new Promise((settle) => {
        resolve = settle;
      });
      waiting = { promise, resolve };
      awaitingDefinition.set(name, waiting);
    }
    return waiting.promise;
  }

  upgrade(root) {
    if (!(root instanceof Node)) throw new TypeError('upgrade needs a node');
    upgradeInside(root);
  }
}
Object.defineProperty(CustomElementRegistry.prototype, Symbol.toStringTag, {
  value: 'CustomElementRegistry',
  configurable: true,
});
defineGlobal('CustomElementRegistry', CustomElementRegistry);
defineGlobal('customElements', Object.create(CustomElementRegistry.prototype));
