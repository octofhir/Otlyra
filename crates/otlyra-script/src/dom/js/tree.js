// What is left of the tree API once the natives are in: the variadic mixins,
// the lists the natives return as arrays, and the platform's exceptions.

// The natives that return lists return arrays; these give them their type.
const listGetter = (Interface, name, asList) => {
  const holder = owner(Interface.prototype, name);
  const native = take(holder, name);
  accessor(holder, name, function () {
    return asList(native.call(this));
  });
};
const listMethod = (Interface, name, asList) => {
  const holder = owner(Interface.prototype, name);
  const native = take(holder, name);
  const wrapped = {
    [name](...args) {
      return asList(native.apply(this, args));
    },
  }[name];
  Object.defineProperty(wrapped, 'length', { value: native.length });
  method(holder, name, wrapped);
};

listGetter(Node, 'childNodes', asNodeList);
for (const Interface of [Element, Document, DocumentFragment]) {
  listGetter(Interface, 'children', asCollection);
  listMethod(Interface, 'querySelectorAll', asNodeList);
}
for (const Interface of [Element, Document]) {
  listMethod(Interface, 'getElementsByTagName', asCollection);
  listMethod(Interface, 'getElementsByTagNameNS', asCollection);
  listMethod(Interface, 'getElementsByClassName', asCollection);
}
listMethod(Document, 'getElementsByName', asNodeList);

// Everything that can throw a DOMException throws a real one.
for (const name of ['appendChild', 'insertBefore', 'replaceChild', 'removeChild', 'cloneNode']) {
  throwsDomExceptions(Node.prototype, name);
}
for (const name of [
  'setAttribute',
  'setAttributeNS',
  'toggleAttribute',
  'querySelector',
  'querySelectorAll',
  'matches',
  'closest',
  'insertAdjacentHTML',
]) {
  throwsDomExceptions(Element.prototype, name);
}
for (const name of [
  'createElement',
  'createElementNS',
  'querySelector',
  'querySelectorAll',
  'importNode',
  'adoptNode',
]) {
  throwsDomExceptions(Document.prototype, name);
}
for (const name of ['querySelector', 'querySelectorAll']) {
  throwsDomExceptions(DocumentFragment.prototype, name);
}
for (const name of ['substringData', 'insertData', 'deleteData', 'replaceData']) {
  throwsDomExceptions(CharacterData.prototype, name);
}
throwsDomExceptions(Text.prototype, 'splitText');

// HTML's markup setters use LegacyNullToEmptyString, and report DOMException
// instances rather than the native binding layer's encoded TypeError.
for (const name of ['innerHTML', 'outerHTML']) {
  const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, name);
  accessor(Element.prototype, name, descriptor.get, function (value) {
    try {
      descriptor.set.call(this, value === null ? '' : String(value));
    } catch (error) {
      throw domException(error);
    }
  });
}

// The old name for `matches`, which a decade of libraries still feature-test.
method(Element.prototype, 'webkitMatchesSelector', Element.prototype.matches);
method(Element.prototype, 'msMatchesSelector', Element.prototype.matches);

// DOM §4.2.6 "convert nodes into a node": nodes stay nodes, anything else
// becomes text, and more than one becomes a fragment holding them in order.
const convertNodes = (context, nodes) => {
  const owner = context.ownerDocument || context;
  const asNode = (value) => (value instanceof Node ? value : owner.createTextNode(String(value)));
  if (nodes.length === 1) return asNode(nodes[0]);
  const fragment = owner.createDocumentFragment();
  for (const node of nodes) callTree('appendChild', fragment, asNode(node));
  return fragment;
};

// ParentNode (DOM §4.2.6), on the three kinds of node that have children.
const PARENT_NODE = {
  prepend(...nodes) {
    callTree('insertBefore', this, convertNodes(this, nodes), this.firstChild);
  },
  append(...nodes) {
    callTree('appendChild', this, convertNodes(this, nodes));
  },
  replaceChildren(...nodes) {
    const node = convertNodes(this, nodes);
    while (this.firstChild) callTree('removeChild', this, this.firstChild);
    callTree('appendChild', this, node);
  },
};

// ChildNode (DOM §4.2.8), on the kinds of node that have a parent to leave.
// Each looks for the sibling to insert against among the siblings that are
// not themselves being moved.
const CHILD_NODE = {
  before(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    let previous = this.previousSibling;
    while (previous && nodes.indexOf(previous) !== -1) previous = previous.previousSibling;
    const node = convertNodes(this, nodes);
    callTree('insertBefore', parent, node, previous ? previous.nextSibling : parent.firstChild);
  },
  after(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    let next = this.nextSibling;
    while (next && nodes.indexOf(next) !== -1) next = next.nextSibling;
    callTree('insertBefore', parent, convertNodes(this, nodes), next);
  },
  replaceWith(...nodes) {
    const parent = this.parentNode;
    if (!parent) return;
    let next = this.nextSibling;
    while (next && nodes.indexOf(next) !== -1) next = next.nextSibling;
    const node = convertNodes(this, nodes);
    if (this.parentNode === parent) callTree('replaceChild', parent, node, this);
    else callTree('insertBefore', parent, node, next);
  },
  remove() {
    const parent = this.parentNode;
    if (parent) callTree('removeChild', parent, this);
  },
};

const mixin = (Interface, members) => {
  for (const name of Object.keys(members)) {
    // Element's own `remove` is native, and stays.
    if (Object.prototype.hasOwnProperty.call(Interface.prototype, name)) continue;
    const member = members[name];
    const wrapped = {
      [name](...args) {
        try {
          return member.apply(this, args);
        } catch (error) {
          throw domException(error);
        }
      },
    }[name];
    Object.defineProperty(wrapped, 'length', { value: 0 });
    method(Interface.prototype, name, wrapped);
  }
};

for (const Interface of [Element, Document, DocumentFragment]) mixin(Interface, PARENT_NODE);
for (const Interface of [Element, CharacterData, DocumentType]) mixin(Interface, CHILD_NODE);

// HTML §3.2.? "insert adjacent": the four places around an element.
const insertAdjacent = (element, where, node) => {
  switch (String(where).toLowerCase()) {
    case 'beforebegin':
      if (!element.parentNode) return null;
      callTree('insertBefore', element.parentNode, node, element);
      return node;
    case 'afterbegin':
      callTree('insertBefore', element, node, element.firstChild);
      return node;
    case 'beforeend':
      callTree('appendChild', element, node);
      return node;
    case 'afterend':
      if (!element.parentNode) return null;
      callTree('insertBefore', element.parentNode, node, element.nextSibling);
      return node;
    default:
      throw new DOMException("'" + where + "' is not a place next to an element", 'SyntaxError');
  }
};

method(Element.prototype, 'insertAdjacentElement', function insertAdjacentElement(where, element) {
  return insertAdjacent(this, where, element);
});
method(Element.prototype, 'insertAdjacentText', function insertAdjacentText(where, data) {
  insertAdjacent(this, where, (this.ownerDocument || document).createTextNode(String(data)));
});

// Members the natives put on Element that belong to one interface.
const moveMember = (name, from, to) => {
  const descriptor = Object.getOwnPropertyDescriptor(from.prototype, name);
  delete from.prototype[name];
  Object.defineProperty(to.prototype, name, descriptor);
};
moveMember('content', Element, HTMLTemplateElement);
moveMember('submit', Element, HTMLFormElement);

// Where relative addresses in the document resolve: the first `<base href>`,
// against the document's own address (HTML §2.4.1).
const documentBase = () => {
  const own = nativeUrl();
  const base = document.querySelector('base[href]');
  if (!base) return own;
  try {
    return new URL(base.getAttribute('href'), own).href;
  } catch (_) {
    return own;
  }
};
accessor(Node.prototype, 'baseURI', function () {
  return documentBase();
});

// The document's facts that are not about any node.
// A document script made has no address of its own, no window and no
// domain; its type is the one it was made with.
const DOCUMENT = {
  get URL() {
    return isMadeDocument(this) ? 'about:blank' : nativeUrl();
  },
  get documentURI() {
    return isMadeDocument(this) ? 'about:blank' : nativeUrl();
  },
  get domain() {
    if (isMadeDocument(this)) return '';
    try {
      return new URL(nativeUrl()).hostname;
    } catch (_) {
      return '';
    }
  },
  get referrer() {
    return '';
  },
  get characterSet() {
    return 'UTF-8';
  },
  get charset() {
    return 'UTF-8';
  },
  get inputEncoding() {
    return 'UTF-8';
  },
  get contentType() {
    return nativeSecondaryType.call(this) ?? 'text/html';
  },
  get defaultView() {
    return isMadeDocument(this) ? null : globalThis;
  },
  get scrollingElement() {
    return this.documentElement;
  },
  get dir() {
    const root = this.documentElement;
    return root ? root.dir : '';
  },
  set dir(value) {
    const root = this.documentElement;
    if (root) root.dir = value;
  },
  get designMode() {
    return 'off';
  },
  set designMode(_value) {},
  get lastModified() {
    const now = new Date();
    const pad = (value) => String(value).padStart(2, '0');
    return (
      pad(now.getMonth() + 1) + '/' + pad(now.getDate()) + '/' + now.getFullYear() + ' ' +
      pad(now.getHours()) + ':' + pad(now.getMinutes()) + ':' + pad(now.getSeconds())
    );
  },
  get forms() {
    return asCollection([...this.querySelectorAll('form')]);
  },
  get images() {
    return asCollection([...this.querySelectorAll('img')]);
  },
  get links() {
    return asCollection([...this.querySelectorAll('a[href], area[href]')]);
  },
  get scripts() {
    return asCollection([...this.querySelectorAll('script')]);
  },
  get embeds() {
    return asCollection([...this.querySelectorAll('embed')]);
  },
  get plugins() {
    return this.embeds;
  },
  get anchors() {
    return asCollection([...this.querySelectorAll('a[name]')]);
  },
  // The focused element, or the body when nothing is focused.
  get activeElement() {
    return this.__otlyraFocused || this.body || this.documentElement;
  },
  hasFocus() {
    return true;
  },
  execCommand() {
    return false;
  },
  queryCommandSupported() {
    return false;
  },
  queryCommandEnabled() {
    return false;
  },
};
for (const name of Object.keys(Object.getOwnPropertyDescriptors(DOCUMENT))) {
  if (Object.prototype.hasOwnProperty.call(Document.prototype, name)) continue;
  Object.defineProperty(
    Document.prototype,
    name,
    Object.assign(Object.getOwnPropertyDescriptor(DOCUMENT, name), { enumerable: true }),
  );
}

// Namespace lookup (DOM §4.4 "locate a namespace" and "locate a namespace
// prefix"): an element's own namespace and prefix first, then its `xmlns`
// declarations, then its parent's.
const XML_NAMESPACE = 'http://www.w3.org/XML/1998/namespace';
const XMLNS_NAMESPACE = 'http://www.w3.org/2000/xmlns/';
const namespaceHolder = (node) => {
  switch (node.nodeType) {
    case Node.ELEMENT_NODE:
      return node;
    case Node.DOCUMENT_NODE:
      return node.documentElement;
    case Node.DOCUMENT_TYPE_NODE:
    case Node.DOCUMENT_FRAGMENT_NODE:
      return null;
    case Node.ATTRIBUTE_NODE:
      return node.ownerElement;
    default:
      return node.parentElement;
  }
};
const locateNamespace = (node, prefix) => {
  for (let element = namespaceHolder(node); element; element = element.parentElement) {
    if (prefix === 'xml') return XML_NAMESPACE;
    if (prefix === 'xmlns') return XMLNS_NAMESPACE;
    if (element.namespaceURI !== null && (element.prefix || null) === prefix) return element.namespaceURI;
    for (const attribute of element.attributes) {
      if (prefix === null ? attribute.name === 'xmlns' : attribute.name === 'xmlns:' + prefix) {
        return attribute.value || null;
      }
    }
  }
  return null;
};
const locatePrefix = (node, namespace) => {
  for (let element = namespaceHolder(node); element; element = element.parentElement) {
    if (element.namespaceURI === namespace && element.prefix) return element.prefix;
    for (const attribute of element.attributes) {
      if (attribute.name.startsWith('xmlns:') && attribute.value === namespace) return attribute.name.slice(6);
    }
  }
  return null;
};
method(Node.prototype, 'lookupNamespaceURI', function lookupNamespaceURI(prefix) {
  return locateNamespace(this, prefix === undefined || prefix === null || prefix === '' ? null : String(prefix));
});
method(Node.prototype, 'lookupPrefix', function lookupPrefix(namespace) {
  if (namespace === undefined || namespace === null || namespace === '') return null;
  return locatePrefix(this, String(namespace));
});
method(Node.prototype, 'isDefaultNamespace', function isDefaultNamespace(namespace) {
  const wanted = namespace === undefined || namespace === null || namespace === '' ? null : String(namespace);
  return locateNamespace(this, null) === wanted;
});
