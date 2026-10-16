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
// Style sheets as script sees them (CSSOM §6.1). One class for both kinds:
// a sheet script constructs and a shadow root adopts, whose text the cascade
// is sent again whenever it changes, and the sheet of a `<style>` element,
// whose rules `insertRule` and `deleteRule` change in place — which is how
// styled-components and emotion style a page once it runs. The rules are
// kept as a list; the text the cascade is given is joined from it once a
// turn, not once per rule, since a page inserts thousands.
const kSheetRules = Symbol('rules');
const kSheetRoots = Symbol('roots');
const kSheetOwner = Symbol('owner');
const kSheetBase = Symbol('base');
const kSheetDirty = Symbol('dirty');
const kSheetCached = Symbol('cached');
const kAdopted = Symbol('adoptedStyleSheets');
const nativeSetCssomText = take(Element.prototype, '__setCssomText');
const sheetText = (sheet) => sheet[kSheetRules].join('\n');
const sendAdopted = (root) => {
  const sheets = root[kAdopted] || [];
  nativeSetAdoptedStyles.call(root, sheets.map(sheetText).join('\n'));
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
class CSSRule {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
}
const makeRule = (cssText, sheet) => {
  const rule = Object.create(CSSRule.prototype);
  const brace = cssText.indexOf('{');
  Object.defineProperties(rule, {
    cssText: { value: cssText, enumerable: true },
    selectorText: { value: brace > 0 && !cssText.startsWith('@') ? cssText.slice(0, brace).trim() : undefined, enumerable: true },
    parentStyleSheet: { value: sheet },
    type: { value: cssText.startsWith('@media') ? 4 : cssText.startsWith('@') ? 0 : 1 },
  });
  return rule;
};
// What changed a sheet has to reach: the roots that adopt it now, and its
// element's cascade at the end of this turn.
const sheetChanged = (sheet) => {
  sheet[kSheetCached] = null;
  for (const root of sheet[kSheetRoots]) sendAdopted(root);
  const owner = sheet[kSheetOwner];
  if (owner && !sheet[kSheetDirty]) {
    sheet[kSheetDirty] = true;
    queueMicrotask(() => {
      sheet[kSheetDirty] = false;
      nativeSetCssomText.call(owner, sheetText(sheet));
    });
  }
};
class CSSStyleSheet {
  constructor(options = {}) {
    this[kSheetRules] = [];
    this[kSheetRoots] = new Set();
    this[kSheetOwner] = null;
    this.disabled = Boolean(options.disabled);
    this.media = options.media || '';
  }
  get ownerNode() {
    return this[kSheetOwner];
  }
  get href() {
    const owner = this[kSheetOwner];
    return owner && owner.localName === 'link' ? owner.href : null;
  }
  get type() {
    return 'text/css';
  }
  get cssRules() {
    if (!this[kSheetCached]) {
      this[kSheetCached] = this[kSheetRules].map((cssText) => makeRule(cssText, this));
    }
    return this[kSheetCached];
  }
  get rules() {
    return this.cssRules;
  }
  replaceSync(text) {
    this[kSheetRules] = splitRules(String(text));
    sheetChanged(this);
  }
  replace(text) {
    this.replaceSync(text);
    return Promise.resolve(this);
  }
  insertRule(rule, index = 0) {
    const rules = this[kSheetRules];
    const at = Number(index) || 0;
    if (at < 0 || at > rules.length) {
      throw new DOMException('The index provided (' + at + ') is larger than the maximum index (' + rules.length + ').', 'IndexSizeError');
    }
    const text = String(rule).trim();
    if (!text.includes('{') && !text.startsWith('@')) {
      throw new DOMException("Failed to parse the rule '" + text + "'.", 'SyntaxError');
    }
    rules.splice(at, 0, text);
    sheetChanged(this);
    return at;
  }
  deleteRule(index) {
    const rules = this[kSheetRules];
    const at = Number(index) || 0;
    if (at < 0 || at >= rules.length) {
      throw new DOMException('The index provided (' + at + ') is outside the range.', 'IndexSizeError');
    }
    rules.splice(at, 1);
    sheetChanged(this);
  }
  addRule(selector, style, index) {
    return this.insertRule(selector + ' { ' + style + ' }', index === undefined ? this[kSheetRules].length : index);
  }
  removeRule(index) {
    this.deleteRule(index === undefined ? 0 : index);
  }
}
defineGlobal('CSSStyleSheet', CSSStyleSheet);
defineGlobal('CSSRule', CSSRule);
defineGlobal('StyleSheet', CSSStyleSheet);

// A `<style>` element's sheet (CSSOM §6.1 "create a CSS style sheet"),
// made from its text the first time script asks and again whenever that
// text has changed since; a `<link rel=stylesheet>`'s, whose rules a page
// does not read (its text is the network's, not script's).
const elementSheets = new WeakMap();
const sheetOf = (element) => {
  if (!element.isConnected) return null;
  const isStyle = element.localName === 'style';
  if (!isStyle) {
    const rel = (element.getAttribute('rel') || '').toLowerCase().split(/\s+/);
    if (!rel.includes('stylesheet')) return null;
  }
  const text = isStyle ? element.textContent : '';
  let sheet = elementSheets.get(element);
  if (!sheet || (isStyle && sheet[kSheetBase] !== text && !sheet[kSheetDirty])) {
    sheet = new CSSStyleSheet({ media: element.getAttribute('media') || '' });
    sheet[kSheetRules] = isStyle ? splitRules(text) : [];
    sheet[kSheetBase] = text;
    sheet[kSheetOwner] = isStyle ? element : null;
    Object.defineProperty(sheet, 'ownerNode', { value: element });
    elementSheets.set(element, sheet);
  }
  return sheet;
};
accessor(HTMLStyleElement.prototype, 'sheet', function () {
  return sheetOf(this);
});
accessor(HTMLLinkElement.prototype, 'sheet', function () {
  return sheetOf(this);
});
// `document.styleSheets` (CSSOM §6.2.2): the sheets of the document's own
// `<style>` and `<link rel=stylesheet>` elements, in tree order.
accessor(Document.prototype, 'styleSheets', function () {
  const sheets = Array.from(this.querySelectorAll('style, link[rel~="stylesheet" i]'))
    .map(sheetOf)
    .filter(Boolean);
  sheets.item = (index) => sheets[index] || null;
  return sheets;
});
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
