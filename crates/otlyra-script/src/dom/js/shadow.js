// Shadow trees (DOM §4.2.2, §4.8): `attachShadow`, the `ShadowRoot` a host
// keeps beside its children, and slots. The browser lays out and styles the
// flat tree, with each shadow root's own rules applying inside it; what is
// here is the script side of the same tree.
//
// Not here yet: event retargeting (a listener outside a shadow tree sees the
// node inside it as the target), `adoptedStyleSheets`, `delegatesFocus` and
// manual slot assignment.

const ShadowRoot = makeInterface('ShadowRoot', DocumentFragment);

const nativeShadowHost = take(DocumentFragment.prototype, '__shadowHost');
const nativeShadowMode = take(DocumentFragment.prototype, '__shadowMode');
const nativeFragmentInnerHTML = take(DocumentFragment.prototype, '__innerHTML');
const nativeFragmentSetInnerHTML = take(DocumentFragment.prototype, '__setInnerHTML');
const nativeSetAdoptedStyles = take(DocumentFragment.prototype, '__setAdoptedStyles');
const nativeAttachShadow = take(Element.prototype, '__attachShadow');
const nativeShadowRoot = take(Element.prototype, '__shadowRoot');
const nativeAssignedSlot = take(Element.prototype, '__assignedSlot');
const nativeAssignedNodes = take(Element.prototype, '__assignedNodes');

accessor(ShadowRoot.prototype, 'host', function () {
  return nativeShadowHost.call(this);
});
accessor(ShadowRoot.prototype, 'mode', function () {
  return nativeShadowMode.call(this);
});
accessor(ShadowRoot.prototype, 'delegatesFocus', function () {
  return false;
});
accessor(ShadowRoot.prototype, 'slotAssignment', function () {
  return 'named';
});
accessor(ShadowRoot.prototype, 'clonable', function () {
  return false;
});
accessor(ShadowRoot.prototype, 'serializable', function () {
  return false;
});
accessor(
  ShadowRoot.prototype,
  'innerHTML',
  function () {
    return nativeFragmentInnerHTML.call(this);
  },
  function (html) {
    const leaving = definedSelector ? Array.from(this.childNodes).flatMap(customInside) : [];
    nativeFragmentSetInnerHTML.call(this, html === null ? '' : String(html));
    if (!definedSelector) return;
    // What the markup made gets the reactions an element's would.
    settleRemoved(leaving);
    const arrived = Array.from(this.childNodes);
    settleInserted(arrived, arrived.map(() => false));
  },
);
accessor(ShadowRoot.prototype, 'activeElement', function () {
  const active = document.activeElement;
  return active && this.contains(active) ? active : null;
});
// Constructed style sheets (CSSOM §6.1.2): a sheet script builds from text
// and a shadow root adopts. What the cascade is given is each adopting
// root's sheets' text, sent again whenever one of them is replaced.
const kSheetText = Symbol('text');
const kSheetRoots = Symbol('roots');
const kAdopted = Symbol('adoptedStyleSheets');
const sendAdopted = (root) => {
  const sheets = root[kAdopted] || [];
  nativeSetAdoptedStyles.call(root, sheets.map((sheet) => sheet[kSheetText]).join('\n'));
};
// The top-level rules of a sheet's text, split where a rule ends.
const splitRules = (text) => {
  const rules = [];
  let depth = 0;
  let start = 0;
  for (let index = 0; index < text.length; index++) {
    const character = text[index];
    if (character === '{') depth++;
    else if (character === '}') {
      depth--;
      if (depth === 0) {
        rules.push(text.slice(start, index + 1).trim());
        start = index + 1;
      }
    } else if (character === ';' && depth === 0) {
      const rule = text.slice(start, index + 1).trim();
      if (rule.startsWith('@')) rules.push(rule);
      start = index + 1;
    }
  }
  return rules.filter((rule) => rule.length > 0);
};
class CSSStyleSheet {
  constructor(options = {}) {
    this[kSheetText] = '';
    this[kSheetRoots] = new Set();
    this.disabled = Boolean(options.disabled);
    this.media = options.media || '';
  }
  get cssRules() {
    return splitRules(this[kSheetText]).map((cssText) => ({ cssText }));
  }
  get rules() {
    return this.cssRules;
  }
  replaceSync(text) {
    this[kSheetText] = String(text);
    for (const root of this[kSheetRoots]) sendAdopted(root);
  }
  replace(text) {
    this.replaceSync(text);
    return Promise.resolve(this);
  }
  insertRule(rule, index = 0) {
    const rules = splitRules(this[kSheetText]);
    const at = Math.max(0, Math.min(Number(index) || 0, rules.length));
    rules.splice(at, 0, String(rule));
    this.replaceSync(rules.join('\n'));
    return at;
  }
  deleteRule(index) {
    const rules = splitRules(this[kSheetText]);
    rules.splice(Number(index) || 0, 1);
    this.replaceSync(rules.join('\n'));
  }
}
defineGlobal('CSSStyleSheet', CSSStyleSheet);
accessor(
  ShadowRoot.prototype,
  'adoptedStyleSheets',
  function () {
    return this[kAdopted] || (this[kAdopted] = []);
  },
  function (sheets) {
    const next = Array.from(sheets || []);
    for (const sheet of next) {
      if (!(sheet instanceof CSSStyleSheet)) {
        throw new TypeError('adoptedStyleSheets takes constructed CSSStyleSheet objects');
      }
    }
    for (const sheet of this[kAdopted] || []) sheet[kSheetRoots].delete(this);
    for (const sheet of next) sheet[kSheetRoots].add(this);
    this[kAdopted] = next;
    sendAdopted(this);
  },
);
// The document's own adopted sheets are kept but not yet applied.
const documentAdopted = [];
accessor(
  Document.prototype,
  'adoptedStyleSheets',
  function () {
    return documentAdopted;
  },
  function (sheets) {
    documentAdopted.splice(0, documentAdopted.length, ...Array.from(sheets || []));
  },
);
method(ShadowRoot.prototype, 'getHTML', function getHTML() {
  return nativeFragmentInnerHTML.call(this);
});
method(ShadowRoot.prototype, 'setHTMLUnsafe', function setHTMLUnsafe(html) {
  nativeFragmentSetInnerHTML.call(this, String(html));
});

// The elements a shadow root may be attached to (DOM §4.9 "valid shadow host
// name"), and any custom element.
const SHADOW_HOSTS = new Set([
  'article', 'aside', 'blockquote', 'body', 'div', 'footer', 'h1', 'h2', 'h3', 'h4',
  'h5', 'h6', 'header', 'main', 'nav', 'p', 'section', 'span',
]);
method(Element.prototype, 'attachShadow', function attachShadow(init) {
  if (!init || (init.mode !== 'open' && init.mode !== 'closed')) {
    throw new TypeError("attachShadow needs a mode of 'open' or 'closed'");
  }
  const name = this.localName;
  if (
    this.namespaceURI !== 'http://www.w3.org/1999/xhtml' ||
    !(SHADOW_HOSTS.has(name) || nativeIsCustomElementName(name))
  ) {
    throw new DOMException('<' + name + '> cannot have a shadow root', 'NotSupportedError');
  }
  let root;
  try {
    root = nativeAttachShadow.call(this, init.mode === 'open');
  } catch (error) {
    throw domException(error);
  }
  shadowHosts.add(this);
  return root;
});
accessor(Element.prototype, 'shadowRoot', function () {
  const root = nativeShadowRoot.call(this);
  return root && root.mode === 'open' ? root : null;
});
accessor(Element.prototype, 'assignedSlot', function () {
  return nativeAssignedSlot.call(this);
});
accessor(Text.prototype, 'assignedSlot', function () {
  const parent = this.parentNode;
  if (!parent || parent.nodeType !== 1) return null;
  const root = nativeShadowRoot.call(parent);
  if (!root) return null;
  // A text node goes to the default slot.
  for (const slot of Array.from(root.querySelectorAll('slot'))) {
    if (!slot.getAttribute('name')) return slot;
  }
  return null;
});
method(HTMLSlotElement.prototype, 'assignedNodes', function assignedNodes(options) {
  const nodes = nativeAssignedNodes.call(this);
  if (nodes.length || !(options && options.flatten)) return nodes;
  return Array.from(this.childNodes);
});
method(HTMLSlotElement.prototype, 'assignedElements', function assignedElements(options) {
  return this.assignedNodes(options).filter((node) => node.nodeType === 1);
});
method(HTMLSlotElement.prototype, 'assign', function assign() {});

// DOM §4.4 `getRootNode`: with `composed`, through each shadow root to its
// host's root.
const nativeGetRootNode = Node.prototype.getRootNode;
method(Node.prototype, 'getRootNode', function getRootNode(options) {
  let root = nativeGetRootNode.call(this);
  if (options && options.composed) {
    while (root instanceof ShadowRoot) root = nativeGetRootNode.call(root.host);
  }
  return root;
});
