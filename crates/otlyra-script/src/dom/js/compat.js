// Interfaces pages feature-test or call on their way somewhere else, kept
// small: each one does what a page that calls it needs in order to go on,
// and says so where that is less than the platform does. A page that cannot
// find one of these throws, and frameworks catch that throw at their root and
// replace the whole page with an error screen — which is worse than every
// shortcut taken here.

// ---------------------------------------------------------------- CSS
// CSS.supports (CSS Conditional §8.1) answers from the property list
// `element.style` already answers `in` from, so the two never disagree.
// Values are not checked: a property we know is one whose values we parse.
const camelProperty = (name) =>
  String(name)
    .trim()
    .toLowerCase()
    .replace(/^-webkit-/, 'webkit-')
    .replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());
const KNOWN_PROPERTIES = new Set(STYLE_PROPERTIES);
const supportsDeclaration = (property, value) => {
  property = String(property).trim();
  if (String(value).trim() === '') return false;
  if (property.startsWith('--')) return true;
  return KNOWN_PROPERTIES.has(camelProperty(property));
};
const supportsCondition = (condition) => {
  const text = String(condition).trim();
  const selector = /^selector\(([^]*)\)$/.exec(text);
  if (selector) {
    try {
      document.querySelector(selector[1]);
      return true;
    } catch (_) {
      return false;
    }
  }
  const declaration = /^\(\s*([-\w]+)\s*:\s*([^]*)\)$/.exec(text);
  return declaration ? supportsDeclaration(declaration[1], declaration[2]) : false;
};
// CSSOM §7.1 `CSS.escape`.
const cssEscape = (value) => {
  const string = String(value);
  let result = '';
  for (let index = 0; index < string.length; index++) {
    const code = string.charCodeAt(index);
    const character = string.charAt(index);
    if (code === 0) result += '�';
    else if (
      (code >= 0x1 && code <= 0x1f) ||
      code === 0x7f ||
      (index === 0 && code >= 0x30 && code <= 0x39) ||
      (index === 1 && code >= 0x30 && code <= 0x39 && string.charCodeAt(0) === 0x2d)
    ) {
      result += '\\' + code.toString(16) + ' ';
    } else if (index === 0 && string.length === 1 && code === 0x2d) {
      result += '\\' + character;
    } else if (code >= 0x80 || code === 0x2d || code === 0x5f || /[0-9A-Za-z]/.test(character)) {
      result += character;
    } else {
      result += '\\' + character;
    }
  }
  return result;
};
defineGlobal(
  'CSS',
  Object.freeze({
    supports(property, value) {
      return arguments.length >= 2 ? supportsDeclaration(property, value) : supportsCondition(property);
    },
    escape: cssEscape,
  }),
);

// ---------------------------------------------------------------- Storage
// `localStorage` and `sessionStorage` (HTML §12.2): kept in memory for as
// long as this document is, which is what a private window gives a page. A
// page reads what it wrote; nothing survives the page.
const storageMaps = new WeakMap();
const storageOf = (storage) => {
  const map = storageMaps.get(storage);
  if (!map) throw new TypeError('Illegal invocation');
  return map;
};
class Storage {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
  get length() {
    return storageOf(this).size;
  }
  key(index) {
    const keys = [...storageOf(this).keys()];
    return index >= 0 && index < keys.length ? keys[index] : null;
  }
  getItem(key) {
    const map = storageOf(this);
    key = String(key);
    return map.has(key) ? map.get(key) : null;
  }
  setItem(key, value) {
    storageOf(this).set(String(key), String(value));
  }
  removeItem(key) {
    storageOf(this).delete(String(key));
  }
  clear() {
    storageOf(this).clear();
  }
}
Object.defineProperty(Storage.prototype, Symbol.toStringTag, { value: 'Storage', configurable: true });
defineGlobal('Storage', Storage);
const makeStorage = () => {
  const storage = Object.create(Storage.prototype);
  storageMaps.set(storage, new Map());
  return storage;
};
defineGlobal('localStorage', makeStorage());
defineGlobal('sessionStorage', makeStorage());

// ---------------------------------------------------------------- Geometry
class DOMRectReadOnly {
  constructor(x = 0, y = 0, width = 0, height = 0) {
    Object.defineProperties(this, {
      x: { value: Number(x), enumerable: true },
      y: { value: Number(y), enumerable: true },
      width: { value: Number(width), enumerable: true },
      height: { value: Number(height), enumerable: true },
    });
  }
  get top() {
    return Math.min(this.y, this.y + this.height);
  }
  get bottom() {
    return Math.max(this.y, this.y + this.height);
  }
  get left() {
    return Math.min(this.x, this.x + this.width);
  }
  get right() {
    return Math.max(this.x, this.x + this.width);
  }
  toJSON() {
    const { x, y, width, height, top, right, bottom, left } = this;
    return { x, y, width, height, top, right, bottom, left };
  }
  static fromRect(rect = {}) {
    return new this(rect.x, rect.y, rect.width, rect.height);
  }
}
class DOMRect extends DOMRectReadOnly {}
defineGlobal('DOMRectReadOnly', DOMRectReadOnly);
defineGlobal('DOMRect', DOMRect);

// ---------------------------------------------------------------- Traversal
// DOM §6: NodeFilter, TreeWalker and NodeIterator, over the tree accessors.
const NODE_FILTER = {
  FILTER_ACCEPT: 1,
  FILTER_REJECT: 2,
  FILTER_SKIP: 3,
  SHOW_ALL: 0xffffffff,
  SHOW_ELEMENT: 0x1,
  SHOW_ATTRIBUTE: 0x2,
  SHOW_TEXT: 0x4,
  SHOW_CDATA_SECTION: 0x8,
  SHOW_PROCESSING_INSTRUCTION: 0x40,
  SHOW_COMMENT: 0x80,
  SHOW_DOCUMENT: 0x100,
  SHOW_DOCUMENT_TYPE: 0x200,
  SHOW_DOCUMENT_FRAGMENT: 0x400,
};
defineGlobal('NodeFilter', Object.freeze({ ...NODE_FILTER }));
const filterNode = (walker, node) => {
  if (!((1 << (node.nodeType - 1)) & walker.whatToShow)) return NODE_FILTER.FILTER_SKIP;
  const filter = walker.filter;
  if (filter === null || filter === undefined) return NODE_FILTER.FILTER_ACCEPT;
  const result = typeof filter === 'function' ? filter(node) : filter.acceptNode(node);
  return Number(result);
};
class TreeWalker {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
  parentNode() {
    let node = this.currentNode;
    while (node && node !== this.root) {
      node = node.parentNode;
      if (node && filterNode(this, node) === NODE_FILTER.FILTER_ACCEPT) {
        this.currentNode = node;
        return node;
      }
    }
    return null;
  }
  firstChild() {
    return this._child(true);
  }
  lastChild() {
    return this._child(false);
  }
  _child(first) {
    let node = first ? this.currentNode.firstChild : this.currentNode.lastChild;
    while (node) {
      const result = filterNode(this, node);
      if (result === NODE_FILTER.FILTER_ACCEPT) {
        this.currentNode = node;
        return node;
      }
      if (result === NODE_FILTER.FILTER_SKIP) {
        const child = first ? node.firstChild : node.lastChild;
        if (child) {
          node = child;
          continue;
        }
      }
      while (node) {
        const sibling = first ? node.nextSibling : node.previousSibling;
        if (sibling) {
          node = sibling;
          break;
        }
        const parent = node.parentNode;
        if (!parent || parent === this.root || parent === this.currentNode) return null;
        node = parent;
      }
    }
    return null;
  }
  nextSibling() {
    return this._sibling(true);
  }
  previousSibling() {
    return this._sibling(false);
  }
  _sibling(next) {
    let node = this.currentNode;
    if (node === this.root) return null;
    for (;;) {
      let sibling = next ? node.nextSibling : node.previousSibling;
      while (sibling) {
        node = sibling;
        const result = filterNode(this, node);
        if (result === NODE_FILTER.FILTER_ACCEPT) {
          this.currentNode = node;
          return node;
        }
        sibling = next ? node.firstChild : node.lastChild;
        if (result === NODE_FILTER.FILTER_REJECT || !sibling) {
          sibling = next ? node.nextSibling : node.previousSibling;
        }
      }
      node = node.parentNode;
      if (!node || node === this.root) return null;
      if (filterNode(this, node) === NODE_FILTER.FILTER_ACCEPT) return null;
    }
  }
  nextNode() {
    let node = this.currentNode;
    let result = NODE_FILTER.FILTER_ACCEPT;
    for (;;) {
      while (result !== NODE_FILTER.FILTER_REJECT && node.firstChild) {
        node = node.firstChild;
        result = filterNode(this, node);
        if (result === NODE_FILTER.FILTER_ACCEPT) {
          this.currentNode = node;
          return node;
        }
      }
      let sibling = null;
      let temporary = node;
      while (temporary) {
        if (temporary === this.root) return null;
        sibling = temporary.nextSibling;
        if (sibling) break;
        temporary = temporary.parentNode;
      }
      if (!sibling) return null;
      node = sibling;
      result = filterNode(this, node);
      if (result === NODE_FILTER.FILTER_ACCEPT) {
        this.currentNode = node;
        return node;
      }
    }
  }
  previousNode() {
    let node = this.currentNode;
    while (node !== this.root) {
      let sibling = node.previousSibling;
      while (sibling) {
        node = sibling;
        let result = filterNode(this, node);
        while (result !== NODE_FILTER.FILTER_REJECT && node.lastChild) {
          node = node.lastChild;
          result = filterNode(this, node);
        }
        if (result === NODE_FILTER.FILTER_ACCEPT) {
          this.currentNode = node;
          return node;
        }
        sibling = node.previousSibling;
      }
      if (node === this.root || !node.parentNode) return null;
      node = node.parentNode;
      if (filterNode(this, node) === NODE_FILTER.FILTER_ACCEPT) {
        this.currentNode = node;
        return node;
      }
    }
    return null;
  }
}
defineGlobal('TreeWalker', TreeWalker);
const makeWalker = (Kind, root, whatToShow, filter) => {
  if (!(root instanceof Node)) throw new TypeError('a tree walker needs a root node');
  const walker = Object.create(Kind.prototype);
  walker.root = root;
  walker.whatToShow = whatToShow === undefined ? NODE_FILTER.SHOW_ALL : whatToShow >>> 0;
  walker.filter = filter === undefined ? null : filter;
  walker.currentNode = root;
  return walker;
};
method(Document.prototype, 'createTreeWalker', function createTreeWalker(root, whatToShow, filter) {
  return makeWalker(TreeWalker, root, whatToShow, filter);
});
class NodeIterator {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
  get referenceNode() {
    return this.currentNode;
  }
  nextNode() {
    if (!this._started) {
      this._started = true;
      if (filterNode(this, this.root) === NODE_FILTER.FILTER_ACCEPT) return this.root;
    }
    for (;;) {
      const node = TreeWalker.prototype.nextNode.call(
        Object.assign(Object.create(TreeWalker.prototype), {
          root: this.root,
          whatToShow: NODE_FILTER.SHOW_ALL,
          filter: null,
          currentNode: this.currentNode,
        }),
      );
      if (!node) return null;
      this.currentNode = node;
      if (filterNode(this, node) === NODE_FILTER.FILTER_ACCEPT) return node;
    }
  }
  previousNode() {
    return null;
  }
  detach() {}
}
defineGlobal('NodeIterator', NodeIterator);
method(Document.prototype, 'createNodeIterator', function createNodeIterator(root, whatToShow, filter) {
  return makeWalker(NodeIterator, root, whatToShow, filter);
});

// ---------------------------------------------------------------- Range and Selection
// Enough of DOM §5 for what pages do with a range they never show: parse
// markup into a fragment, measure nothing, and select nothing.
class Range {
  constructor() {
    this.startContainer = document;
    this.startOffset = 0;
    this.endContainer = document;
    this.endOffset = 0;
  }
  get collapsed() {
    return this.startContainer === this.endContainer && this.startOffset === this.endOffset;
  }
  get commonAncestorContainer() {
    let node = this.startContainer;
    while (node && !node.contains(this.endContainer)) node = node.parentNode;
    return node || document;
  }
  setStart(node, offset) {
    this.startContainer = node;
    this.startOffset = Number(offset) || 0;
  }
  setEnd(node, offset) {
    this.endContainer = node;
    this.endOffset = Number(offset) || 0;
  }
  setStartBefore(node) {
    this.setStart(node.parentNode, Array.prototype.indexOf.call(node.parentNode.childNodes, node));
  }
  setStartAfter(node) {
    this.setStart(node.parentNode, Array.prototype.indexOf.call(node.parentNode.childNodes, node) + 1);
  }
  setEndBefore(node) {
    this.setEnd(node.parentNode, Array.prototype.indexOf.call(node.parentNode.childNodes, node));
  }
  setEndAfter(node) {
    this.setEnd(node.parentNode, Array.prototype.indexOf.call(node.parentNode.childNodes, node) + 1);
  }
  selectNode(node) {
    this.setStartBefore(node);
    this.setEndAfter(node);
  }
  selectNodeContents(node) {
    this.setStart(node, 0);
    this.setEnd(node, node.childNodes.length);
  }
  collapse(toStart) {
    if (toStart) this.setEnd(this.startContainer, this.startOffset);
    else this.setStart(this.endContainer, this.endOffset);
  }
  cloneRange() {
    const copy = new Range();
    copy.setStart(this.startContainer, this.startOffset);
    copy.setEnd(this.endContainer, this.endOffset);
    return copy;
  }
  detach() {}
  // HTML's fragment parsing algorithm, through a template: what Turbo,
  // htmx and a hundred others use to turn a response into nodes.
  createContextualFragment(markup) {
    const template = document.createElement('template');
    template.innerHTML = String(markup);
    const fragment = document.createDocumentFragment();
    const content = template.content || template;
    while (content.firstChild) fragment.appendChild(content.firstChild);
    return fragment;
  }
  getBoundingClientRect() {
    const node = this.startContainer;
    return node && node.nodeType === 1 ? node.getBoundingClientRect() : new DOMRect();
  }
  getClientRects() {
    return [];
  }
  toString() {
    return this.startContainer === this.endContainer && this.startContainer.nodeType === 3
      ? this.startContainer.data.slice(this.startOffset, this.endOffset)
      : '';
  }
}
defineGlobal('Range', Range);
method(Document.prototype, 'createRange', function createRange() {
  return new Range();
});
class Selection {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
  get rangeCount() {
    return 0;
  }
  get isCollapsed() {
    return true;
  }
  get anchorNode() {
    return null;
  }
  get focusNode() {
    return null;
  }
  get anchorOffset() {
    return 0;
  }
  get focusOffset() {
    return 0;
  }
  get type() {
    return 'None';
  }
  getRangeAt() {
    throw new DOMException('the selection has no ranges', 'IndexSizeError');
  }
  addRange() {}
  removeRange() {}
  removeAllRanges() {}
  empty() {}
  collapse() {}
  collapseToStart() {}
  collapseToEnd() {}
  selectAllChildren() {}
  containsNode() {
    return false;
  }
  toString() {
    return '';
  }
}
defineGlobal('Selection', Selection);
const theSelection = Object.create(Selection.prototype);
defineGlobal('getSelection', function getSelection() {
  return theSelection;
});
method(Document.prototype, 'getSelection', function getSelection() {
  return theSelection;
});

// ---------------------------------------------------------------- Elements
method(Element.prototype, 'scrollIntoView', function scrollIntoView() {
  const rect = this.getBoundingClientRect();
  if (rect.width === 0 && rect.height === 0) return;
  scrollTo(scrollX + rect.left, scrollY + rect.top);
});
method(Element.prototype, 'scrollIntoViewIfNeeded', function scrollIntoViewIfNeeded() {
  const rect = this.getBoundingClientRect();
  if (rect.top < 0 || rect.bottom > innerHeight) this.scrollIntoView();
});
method(Element.prototype, 'checkVisibility', function checkVisibility() {
  if (!this.isConnected) return false;
  const rect = this.getBoundingClientRect();
  return rect.width > 0 || rect.height > 0;
});
for (const name of ['setPointerCapture', 'releasePointerCapture']) {
  method(Element.prototype, name, function () {});
}
method(Element.prototype, 'hasPointerCapture', function hasPointerCapture() {
  return false;
});
// Web Animations: nothing moves, so an animation is over as soon as it is
// made, and its last keyframe is where the element is left only if the page
// asked to keep it (`fill: forwards`), which it applies itself.
class Animation extends EventTarget {
  constructor() {
    super();
    this.playState = 'finished';
    this.currentTime = null;
    this.playbackRate = 1;
    this.onfinish = null;
    this.oncancel = null;
    this.finished = Promise.resolve(this);
    this.ready = Promise.resolve(this);
    setTimeout(() => {
      const event = new Event('finish');
      if (typeof this.onfinish === 'function') this.onfinish.call(this, event);
      this.dispatchEvent(event);
    }, 0);
  }
  play() {}
  pause() {}
  finish() {}
  cancel() {
    this.playState = 'idle';
  }
  reverse() {}
  commitStyles() {}
  persist() {}
  updatePlaybackRate(rate) {
    this.playbackRate = Number(rate);
  }
}
defineGlobal('Animation', Animation);
method(Element.prototype, 'animate', function animate() {
  return new Animation();
});
method(Element.prototype, 'getAnimations', function getAnimations() {
  return [];
});
method(Document.prototype, 'getAnimations', function getAnimations() {
  return [];
});

// ---------------------------------------------------------------- Window
// Dialogs nobody can answer: a page that asks gets the answer a person who
// dismissed the dialog gives.
defineGlobal('alert', function alert(message) {
  console.warn('alert:', String(message === undefined ? '' : message));
});
defineGlobal('confirm', function confirm(message) {
  console.warn('confirm:', String(message === undefined ? '' : message));
  return false;
});
defineGlobal('prompt', function prompt(message) {
  console.warn('prompt:', String(message === undefined ? '' : message));
  return null;
});
// A new browsing context is not one script may open here yet.
defineGlobal('open', function open() {
  return null;
});
defineGlobal('print', function print() {});

// Performance Timeline's observer, with no entries to deliver.
class PerformanceObserver {
  constructor(callback) {
    if (typeof callback !== 'function') throw new TypeError('PerformanceObserver needs a callback');
  }
  observe() {}
  disconnect() {}
  takeRecords() {
    return [];
  }
  static get supportedEntryTypes() {
    return [];
  }
}
defineGlobal('PerformanceObserver', PerformanceObserver);

// The visual viewport is the layout viewport here: no pinch zoom.
const visualViewport = new EventTarget();
Object.defineProperties(visualViewport, {
  width: { get: () => innerWidth, enumerable: true },
  height: { get: () => innerHeight, enumerable: true },
  scale: { value: 1, enumerable: true },
  offsetLeft: { value: 0, enumerable: true },
  offsetTop: { value: 0, enumerable: true },
  pageLeft: { get: () => scrollX, enumerable: true },
  pageTop: { get: () => scrollY, enumerable: true },
  onresize: { value: null, writable: true },
  onscroll: { value: null, writable: true },
});
defineGlobal('visualViewport', visualViewport);

// `document.fonts`: the faces a page names load with the page, before its
// load event, so the set is always loaded by the time script asks.
const fontFaceSet = new EventTarget();
Object.defineProperties(fontFaceSet, {
  status: { value: 'loaded', enumerable: true },
  ready: { value: Promise.resolve(fontFaceSet), enumerable: true },
  size: { value: 0, enumerable: true },
  check: { value: () => true },
  load: { value: () => Promise.resolve([]) },
  forEach: { value: () => {} },
  has: { value: () => false },
  add: { value: function () { return this; } },
  delete: { value: () => false },
  clear: { value: () => {} },
  onloading: { value: null, writable: true },
  onloadingdone: { value: null, writable: true },
  onloadingerror: { value: null, writable: true },
});
Object.defineProperty(Document.prototype, 'fonts', {
  get() {
    return fontFaceSet;
  },
  enumerable: true,
  configurable: true,
});

// Analytics still asks; no browser has said yes in years.
define(navigator, 'javaEnabled', function javaEnabled() {
  return false;
});

// ---------------------------------------------------------------- Wrappers
// The prototype each interface's wrappers are made with, as the glue left
// it. A page may put something else at `window.HTMLElement` — the custom
// elements "native shim" every Web Components polyfill ships replaces it
// with a plain function — and a node met after that is still an
// `HTMLElement` of this realm, with its members. `identity.rs` reads this
// table rather than the global.
const interfacePrototypes = Object.create(null);
for (const name of [
  ...INTERFACE_TREE.map(([interfaceName]) => interfaceName),
  'Node',
  'Element',
  'CharacterData',
  'Text',
  'Comment',
  'Document',
  'HTMLDocument',
  'DocumentFragment',
  'DocumentType',
]) {
  const Interface = globalThis[name];
  if (Interface && Interface.prototype) interfacePrototypes[name] = Interface.prototype;
}
Object.defineProperty(globalThis, '__otlyraInterfacePrototypes', {
  value: Object.freeze(interfacePrototypes),
  writable: false,
  enumerable: false,
  configurable: false,
});
