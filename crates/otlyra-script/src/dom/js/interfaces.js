// The interface tree. Every element is made as one of these — see
// `interfaces.rs` for which — and every one is a subclass of the native class
// whose data its wrapper carries, so the native members work on it and only
// the prototype differs.

// EventTarget sits under Node, as it does on the platform: `node instanceof
// EventTarget` is a thing pages ask.
Object.setPrototypeOf(Node.prototype, EventTarget.prototype);
Object.setPrototypeOf(Node, EventTarget);

// DOM §4.4's constants, on the interface object and on its prototype.
const NODE_CONSTANTS = {
  ELEMENT_NODE: 1,
  ATTRIBUTE_NODE: 2,
  TEXT_NODE: 3,
  CDATA_SECTION_NODE: 4,
  ENTITY_REFERENCE_NODE: 5,
  ENTITY_NODE: 6,
  PROCESSING_INSTRUCTION_NODE: 7,
  COMMENT_NODE: 8,
  DOCUMENT_NODE: 9,
  DOCUMENT_TYPE_NODE: 10,
  DOCUMENT_FRAGMENT_NODE: 11,
  NOTATION_NODE: 12,
  DOCUMENT_POSITION_DISCONNECTED: 0x01,
  DOCUMENT_POSITION_PRECEDING: 0x02,
  DOCUMENT_POSITION_FOLLOWING: 0x04,
  DOCUMENT_POSITION_CONTAINS: 0x08,
  DOCUMENT_POSITION_CONTAINED_BY: 0x10,
  DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: 0x20,
};
for (const name of Object.keys(NODE_CONSTANTS)) {
  const constant = { value: NODE_CONSTANTS[name], enumerable: true };
  Object.defineProperty(Node, name, constant);
  Object.defineProperty(Node.prototype, name, constant);
}

// What `new` on `HTMLElement` does. Nothing may construct one, until
// `custom_elements.js` replaces this with the HTML element constructor, which
// lets a defined custom element's class through (HTML §3.2.3).
let htmlElementConstructor = () => {
  throw new TypeError('Illegal constructor');
};

// An interface nobody may construct: the class exists so that `instanceof`
// and the prototype chain work, and `new` throws, as the platform's do.
// `HTMLElement` is the exception a custom element's class reaches through
// `super()`.
const makeInterface = (name, Parent) => {
  const Interface =
    name === 'HTMLElement'
      ? class extends Parent {
          constructor() {
            return htmlElementConstructor(new.target);
          }
        }
      : class extends Parent {
          constructor() {
            throw new TypeError('Illegal constructor');
          }
        };
  Object.defineProperty(Interface, 'name', { value: name });
  Object.defineProperty(Interface.prototype, Symbol.toStringTag, {
    value: name,
    configurable: true,
  });
  defineGlobal(name, Interface);
  return Interface;
};

// The three node kinds a page may construct. Each is a subclass of the native
// class whose constructor makes the node through the document — so the node
// gets its wrapper the way every other node does, and the object `new`
// returns is the one every later lookup finds — and the subclass takes the
// native one's place as the global, which is the class wrappers are made as.
// It must be a class: the engine finds the prototype a wrapper is made with
// through the global, and a plain function's is not one it reads.
const constructible = (Native, make) => {
  const Interface = class extends Native {
    constructor(...args) {
      return make(...args);
    }
  };
  Object.defineProperty(Interface, 'name', { value: Native.name });
  defineGlobal(Native.name, Interface);
};

constructible(Text, (data = '') => document.createTextNode(String(data)));
constructible(Comment, (data = '') => document.createComment(String(data)));
constructible(DocumentFragment, () => document.createDocumentFragment());

for (const [name, parent] of INTERFACE_TREE) {
  makeInterface(name, globalThis[parent]);
}
