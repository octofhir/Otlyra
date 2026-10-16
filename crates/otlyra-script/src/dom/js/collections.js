// The list types the platform hands back.
//
// A list here is an array underneath, so indexing and `length` cost what they
// cost on an array, with the list's own prototype on top: `item`, `forEach` and
// iteration, and none of `map` or `push`, which is what a page that tells a
// NodeList from an array by its methods expects to find. They are snapshots —
// a `childNodes` taken before an append does not grow — which is what a page
// building lists by query almost always means, and the live lists of the
// specification are a cost to come back to when a page shows they matter.

const listInterface = (name) => {
  const List = makeInterface(name, Object);
  Object.setPrototypeOf(List.prototype, Object.prototype);
  method(List.prototype, 'item', function item(index) {
    const at = Number(index) >>> 0;
    return at < this.length ? this[at] : null;
  });
  method(List.prototype, Symbol.iterator, Array.prototype.values);
  return List;
};

const NodeList = listInterface('NodeList');
method(NodeList.prototype, 'forEach', Array.prototype.forEach);
method(NodeList.prototype, 'entries', Array.prototype.entries);
method(NodeList.prototype, 'keys', Array.prototype.keys);
method(NodeList.prototype, 'values', Array.prototype.values);

const HTMLCollection = listInterface('HTMLCollection');
// The element whose id, or failing that whose name, is `name` (DOM §4.2.10.2).
method(HTMLCollection.prototype, 'namedItem', function namedItem(name) {
  const key = String(name);
  if (key === '') return null;
  for (let index = 0; index < this.length; index++) {
    if (this[index].id === key) return this[index];
  }
  for (let index = 0; index < this.length; index++) {
    const element = this[index];
    if (element.namespaceURI === 'http://www.w3.org/1999/xhtml' && element.getAttribute('name') === key) {
      return element;
    }
  }
  return null;
});

const asNodeList = (array) => Object.setPrototypeOf(array, NodeList.prototype);
const asCollection = (array) => Object.setPrototypeOf(array, HTMLCollection.prototype);

// A token list: a live view of one attribute, read when asked and written when
// changed, because the attribute is where style and selectors look.
const DOMTokenList = makeInterface('DOMTokenList', Object);
Object.setPrototypeOf(DOMTokenList.prototype, Object.prototype);
const kTokenOwner = Symbol('owner');
const kTokenAttribute = Symbol('attribute');
const kTokenSupported = Symbol('supported');

const tokensOf = (list) => {
  const value = list[kTokenOwner].getAttribute(list[kTokenAttribute]);
  if (!value) return [];
  const seen = [];
  for (const token of value.split(/[\t\n\f\r ]+/)) {
    if (token && seen.indexOf(token) === -1) seen.push(token);
  }
  return seen;
};

const writeTokens = (list, tokens) => {
  list[kTokenOwner].setAttribute(list[kTokenAttribute], tokens.join(' '));
};

// DOM §7.1 "validation": a token is not empty and holds no white space.
const checkToken = (token) => {
  const text = String(token);
  if (text === '') throw new DOMException('the token is empty', 'SyntaxError');
  if (/[\t\n\f\r ]/.test(text)) {
    throw new DOMException('the token contains white space', 'InvalidCharacterError');
  }
  return text;
};

const DOM_TOKEN_LIST = {
  get length() {
    return tokensOf(this).length;
  },
  get value() {
    return this[kTokenOwner].getAttribute(this[kTokenAttribute]) || '';
  },
  set value(next) {
    this[kTokenOwner].setAttribute(this[kTokenAttribute], String(next));
  },
  item(index) {
    const tokens = tokensOf(this);
    const at = Number(index) >>> 0;
    return at < tokens.length ? tokens[at] : null;
  },
  contains(token) {
    return tokensOf(this).indexOf(String(token)) !== -1;
  },
  add(...tokens) {
    const names = tokens.map(checkToken);
    const current = tokensOf(this);
    for (const name of names) if (current.indexOf(name) === -1) current.push(name);
    writeTokens(this, current);
  },
  remove(...tokens) {
    const names = tokens.map(checkToken);
    writeTokens(this, tokensOf(this).filter((token) => names.indexOf(token) === -1));
  },
  toggle(token, force) {
    const name = checkToken(token);
    const has = this.contains(name);
    const wanted = force === undefined ? !has : Boolean(force);
    if (wanted !== has) {
      if (wanted) this.add(name);
      else this.remove(name);
    }
    return wanted;
  },
  replace(from, to) {
    const old = checkToken(from);
    const next = checkToken(to);
    const tokens = tokensOf(this);
    const at = tokens.indexOf(old);
    if (at === -1) return false;
    const already = tokens.indexOf(next);
    tokens[at] = next;
    writeTokens(this, already === -1 || already === at ? tokens : tokens.filter((t, i) => t !== next || i === at));
    return true;
  },
  supports(token) {
    const supported = this[kTokenSupported];
    if (!supported) throw new TypeError('this token list has no supported tokens');
    return supported.indexOf(String(token).toLowerCase()) !== -1;
  },
  toString() {
    return this.value;
  },
  forEach(callback, thisArg) {
    tokensOf(this).forEach(callback, thisArg);
  },
  entries() {
    return tokensOf(this).entries();
  },
  keys() {
    return tokensOf(this).keys();
  },
  values() {
    return tokensOf(this).values();
  },
  [Symbol.iterator]() {
    return tokensOf(this)[Symbol.iterator]();
  },
};
Object.defineProperties(DOMTokenList.prototype, Object.getOwnPropertyDescriptors(DOM_TOKEN_LIST));

// One list per element and attribute, kept on the element, so that
// `el.classList === el.classList` — a page compares them, and caches them.
const tokenList = (Interface, property, attribute, supported) => {
  const key = Symbol(property);
  accessor(
    Interface.prototype,
    property,
    function () {
      let list = this[key];
      if (!list) {
        list = Object.create(DOMTokenList.prototype);
        list[kTokenOwner] = this;
        list[kTokenAttribute] = attribute;
        list[kTokenSupported] = supported;
        Object.defineProperty(this, key, { value: list });
      }
      return list;
    },
    function (value) {
      this.setAttribute(attribute, String(value));
    },
  );
};

tokenList(Element, 'classList', 'class');

// `attributes`: one Attr per attribute, as a list with names to look them up
// by. An Attr reads its value through its element, so it is never stale.
const Attr = makeInterface('Attr', Node);
const NamedNodeMap = listInterface('NamedNodeMap');
const kAttrOwner = Symbol('ownerElement');
const kAttrRecord = Symbol('record');

const makeAttr = (owner, record) => {
  const attr = Object.create(Attr.prototype);
  attr[kAttrOwner] = owner;
  attr[kAttrRecord] = record;
  return attr;
};

const ATTR = {
  get name() {
    return this[kAttrRecord].name;
  },
  get localName() {
    return this[kAttrRecord].localName;
  },
  get namespaceURI() {
    return this[kAttrRecord].namespaceURI;
  },
  get prefix() {
    return this[kAttrRecord].prefix;
  },
  get ownerElement() {
    return this[kAttrOwner];
  },
  get specified() {
    return true;
  },
  get value() {
    const record = this[kAttrRecord];
    const owner = this[kAttrOwner];
    return owner ? owner.getAttributeNS(record.namespaceURI, record.localName) ?? record.value : record.value;
  },
  set value(next) {
    const record = this[kAttrRecord];
    const owner = this[kAttrOwner];
    if (owner) owner.setAttributeNS(record.namespaceURI, record.name, String(next));
    else record.value = String(next);
  },
  get nodeType() {
    return 2;
  },
  get nodeName() {
    return this.name;
  },
  get nodeValue() {
    return this.value;
  },
  set nodeValue(next) {
    this.value = next;
  },
  get textContent() {
    return this.value;
  },
  set textContent(next) {
    this.value = next;
  },
  get parentNode() {
    return null;
  },
};
Object.defineProperties(Attr.prototype, Object.getOwnPropertyDescriptors(ATTR));

const NAMED_NODE_MAP = {
  getNamedItem(name) {
    let key = String(name);
    const owner = this.length ? this[0][kAttrOwner] : null;
    if (owner && owner.namespaceURI === 'http://www.w3.org/1999/xhtml') key = key.toLowerCase();
    for (let index = 0; index < this.length; index++) {
      if (this[index].name === key) return this[index];
    }
    return null;
  },
  getNamedItemNS(namespace, localName) {
    const ns = namespace === '' ? null : namespace;
    for (let index = 0; index < this.length; index++) {
      const attr = this[index];
      if (attr.namespaceURI === ns && attr.localName === String(localName)) return attr;
    }
    return null;
  },
  removeNamedItem(name) {
    const attr = this.getNamedItem(name);
    if (!attr) throw new DOMException('no attribute named ' + name, 'NotFoundError');
    attr[kAttrOwner].removeAttribute(attr.name);
    return attr;
  },
  setNamedItem(attr) {
    const owner = this[kAttrOwner];
    owner.setAttributeNS(attr.namespaceURI, attr.name, attr.value);
    return null;
  },
  setNamedItemNS(attr) {
    return this.setNamedItem(attr);
  },
};
Object.defineProperties(NamedNodeMap.prototype, Object.getOwnPropertyDescriptors(NAMED_NODE_MAP));

accessor(Element.prototype, 'attributes', function () {
  const map = asNamedNodeMap(nativeAttributeRecords.call(this).map((record) => makeAttr(this, record)));
  Object.defineProperty(map, kAttrOwner, { value: this });
  // Each attribute is also a property by its qualified name (DOM §4.9.1's
  // supported property names), unless the map already answers to that
  // name: `element.attributes.class`, `attributes['onclick']`.
  for (const attr of map) {
    const name = attr.name;
    if (name in map || /^\d+$/.test(name)) continue;
    Object.defineProperty(map, name, { value: attr, configurable: true });
  }
  return map;
});

function asNamedNodeMap(array) {
  return Object.setPrototypeOf(array, NamedNodeMap.prototype);
}

method(Element.prototype, 'getAttributeNode', function getAttributeNode(name) {
  return this.attributes.getNamedItem(name);
});
method(Element.prototype, 'getAttributeNodeNS', function getAttributeNodeNS(namespace, localName) {
  return this.attributes.getNamedItemNS(namespace, localName);
});
method(Element.prototype, 'setAttributeNode', function setAttributeNode(attr) {
  const previous = this.getAttributeNodeNS(attr.namespaceURI, attr.localName);
  this.setAttributeNS(attr.namespaceURI, attr.name, attr.value);
  return previous;
});
method(Element.prototype, 'setAttributeNodeNS', function setAttributeNodeNS(attr) {
  return this.setAttributeNode(attr);
});
// DOM §4.9 removeAttributeNode: the attribute must be this element's; it
// leaves the element and keeps its value, as an attribute of no element.
method(Element.prototype, 'removeAttributeNode', function removeAttributeNode(attr) {
  if (
    !(attr instanceof Attr) ||
    attr[kAttrOwner] !== this ||
    this.getAttributeNS(attr.namespaceURI, attr.localName) === null
  ) {
    throw new DOMException('the attribute is not an attribute of this element', 'NotFoundError');
  }
  const value = attr.value;
  this.removeAttributeNS(attr.namespaceURI, attr.localName);
  attr[kAttrRecord] = { ...attr[kAttrRecord], value };
  attr[kAttrOwner] = null;
  return attr;
});
method(Document.prototype, 'createAttribute', function createAttribute(name) {
  const local = String(name).toLowerCase();
  return makeAttr(null, { name: local, localName: local, namespaceURI: null, prefix: null, value: '' });
});

// `dataset`: the `data-*` attributes under the names a script spells them
// with (HTML §3.2.6.6). A proxy, because the property names are the page's.
const DOMStringMap = makeInterface('DOMStringMap', Object);
Object.setPrototypeOf(DOMStringMap.prototype, Object.prototype);

const dataAttribute = (property) =>
  'data-' + property.replace(/[A-Z]/g, (letter) => '-' + letter.toLowerCase());
const dataProperty = (attribute) =>
  attribute.slice(5).replace(/-([a-z])/g, (_all, letter) => letter.toUpperCase());

const kDataset = Symbol('dataset');
const datasetOf = (element) =>
  new Proxy(Object.create(DOMStringMap.prototype), {
    get(target, property, receiver) {
      if (typeof property !== 'string') return Reflect.get(target, property, receiver);
      const value = element.getAttribute(dataAttribute(property));
      return value === null ? Reflect.get(target, property, receiver) : value;
    },
    set(target, property, value, receiver) {
      if (typeof property !== 'string') return Reflect.set(target, property, value, receiver);
      if (/-[a-z]/.test(property)) {
        throw new DOMException("'" + property + "' is not a valid dataset name", 'SyntaxError');
      }
      element.setAttribute(dataAttribute(property), String(value));
      return true;
    },
    has(target, property) {
      if (typeof property !== 'string') return Reflect.has(target, property);
      return element.hasAttribute(dataAttribute(property));
    },
    deleteProperty(target, property) {
      if (typeof property === 'string') element.removeAttribute(dataAttribute(property));
      return true;
    },
    ownKeys() {
      return element
        .getAttributeNames()
        .filter((name) => name.startsWith('data-'))
        .map(dataProperty);
    },
    getOwnPropertyDescriptor(target, property) {
      if (typeof property !== 'string') return Reflect.getOwnPropertyDescriptor(target, property);
      const value = element.getAttribute(dataAttribute(property));
      if (value === null) return undefined;
      return { value, writable: true, enumerable: true, configurable: true };
    },
  });

for (const Interface of [HTMLElement, SVGElement, MathMLElement]) {
  accessor(Interface.prototype, 'dataset', function () {
    let map = this[kDataset];
    if (!map) {
      map = datasetOf(this);
      Object.defineProperty(this, kDataset, { value: map });
    }
    return map;
  });
}
