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
// `localStorage` and `sessionStorage` (HTML §12.2). The browser keeps the
// items: `localStorage` for every document of the origin in every tab, and
// `sessionStorage` for the documents of one tab. A document with no origin to
// share, or no browser, keeps its own. Items are also the object's own
// properties (`localStorage.token`), which is how a good many pages read
// them, so each Storage is a proxy over a plain object.
const storageNatives = {
  length: take(Document, '__storageLength'),
  key: take(Document, '__storageKey'),
  get: take(Document, '__storageGet'),
  set: take(Document, '__storageSet'),
  remove: take(Document, '__storageRemove'),
  clear: take(Document, '__storageClear'),
};
const storageAreas = new WeakMap();
const storageOf = (storage) => {
  const area = storageAreas.get(storage);
  if (area === undefined) throw new TypeError('Illegal invocation');
  return area;
};
class Storage {
  constructor() {
    throw new TypeError('Illegal constructor');
  }
  get length() {
    return storageNatives.length(storageOf(this));
  }
  key(index) {
    const area = storageOf(this);
    if (arguments.length < 1) throw new TypeError("Failed to execute 'key' on 'Storage': 1 argument required");
    return storageNatives.key(area, Number(index) >>> 0) ?? null;
  }
  getItem(key) {
    const area = storageOf(this);
    if (arguments.length < 1) throw new TypeError("Failed to execute 'getItem' on 'Storage': 1 argument required");
    return storageNatives.get(area, String(key)) ?? null;
  }
  setItem(key, value) {
    const area = storageOf(this);
    if (arguments.length < 2) throw new TypeError("Failed to execute 'setItem' on 'Storage': 2 arguments required");
    try {
      storageNatives.set(area, String(key), String(value));
    } catch (error) {
      throw domException(error);
    }
  }
  removeItem(key) {
    const area = storageOf(this);
    if (arguments.length < 1) throw new TypeError("Failed to execute 'removeItem' on 'Storage': 1 argument required");
    storageNatives.remove(area, String(key));
  }
  clear() {
    storageNatives.clear(storageOf(this));
  }
}
Object.defineProperty(Storage.prototype, Symbol.toStringTag, { value: 'Storage', configurable: true });
defineGlobal('Storage', Storage);
// A named item is a property only where the prototype does not have one by
// that name (WebIDL's legacy override-builtins is not set on Storage).
const storageItemName = (key) => typeof key === 'string' && !(key in Storage.prototype);
const makeStorage = (area) => {
  const target = Object.create(Storage.prototype);
  const keys = () => {
    const out = [];
    const length = storageNatives.length(area);
    for (let index = 0; index < length; index += 1) out.push(storageNatives.key(area, index));
    return out;
  };
  const storage = new Proxy(target, {
    get(target, key, receiver) {
      if (storageItemName(key)) {
        const value = storageNatives.get(area, key);
        if (value != null) return value;
      }
      return Reflect.get(target, key, receiver === storage ? target : receiver);
    },
    set(target, key, value, receiver) {
      if (storageItemName(key) && receiver === storage) {
        try {
          storageNatives.set(area, key, String(value));
        } catch (error) {
          throw domException(error);
        }
        return true;
      }
      return Reflect.set(target, key, value, receiver === storage ? target : receiver);
    },
    has(target, key) {
      if (storageItemName(key) && storageNatives.get(area, key) != null) return true;
      return Reflect.has(target, key);
    },
    deleteProperty(target, key) {
      if (storageItemName(key) && storageNatives.get(area, key) != null) {
        storageNatives.remove(area, key);
        return true;
      }
      return Reflect.deleteProperty(target, key);
    },
    defineProperty(target, key, descriptor) {
      if (storageItemName(key) && 'value' in descriptor) {
        storageNatives.set(area, key, String(descriptor.value));
        return true;
      }
      return Reflect.defineProperty(target, key, descriptor);
    },
    ownKeys(target) {
      const own = Reflect.ownKeys(target);
      return [...keys().filter((key) => !own.includes(key)), ...own];
    },
    getOwnPropertyDescriptor(target, key) {
      if (storageItemName(key)) {
        const value = storageNatives.get(area, key);
        if (value != null) return { value, writable: true, enumerable: true, configurable: true };
      }
      return Reflect.getOwnPropertyDescriptor(target, key);
    },
  });
  storageAreas.set(storage, area);
  storageAreas.set(target, area);
  return storage;
};
defineGlobal('localStorage', makeStorage(0));
defineGlobal('sessionStorage', makeStorage(1));

// ---------------------------------------------------------------- Path2D
// A path for a 2D canvas (HTML §4.12.5.1.16). It keeps the commands it is
// given, and `canvas.js` replays them into the bitmap's own path when one is
// filled, stroked or clipped to.
const kPathCommands = Symbol('pathCommands');
class Path2D {
  constructor(path) {
    this[kPathCommands] = path instanceof Path2D ? [...path[kPathCommands]] : [];
    if (typeof path === 'string') this[kPathCommands].push(['svg', path]);
  }
  addPath(path) {
    if (!(path instanceof Path2D)) throw new TypeError("Failed to execute 'addPath' on 'Path2D': parameter 1 is not of type 'Path2D'.");
    this[kPathCommands].push(...path[kPathCommands]);
  }
}
for (const name of ['closePath', 'moveTo', 'lineTo', 'bezierCurveTo', 'quadraticCurveTo', 'arc', 'arcTo', 'ellipse', 'rect', 'roundRect']) {
  Object.defineProperty(Path2D.prototype, name, {
    value: {
      [name](...args) {
        this[kPathCommands].push([name, ...args]);
      },
    }[name],
    writable: true,
    configurable: true,
  });
}
defineGlobal('Path2D', Path2D);

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

// ---------------------------------------------------------------- Matrices
// Geometry Interfaces §4–6: DOMPoint and DOMMatrix, which pages build from
// `getComputedStyle(element).transform` to read or compose a transform.
// Values are kept column-major as the specification names them, m11..m44.
class DOMPointReadOnly {
  constructor(x = 0, y = 0, z = 0, w = 1) {
    this.x = Number(x);
    this.y = Number(y);
    this.z = Number(z);
    this.w = Number(w);
  }
  static fromPoint(point = {}) {
    return new this(point.x, point.y, point.z, point.w);
  }
  matrixTransform(matrix) {
    return DOMMatrixReadOnly.fromMatrix(matrix).transformPoint(this);
  }
  toJSON() {
    return { x: this.x, y: this.y, z: this.z, w: this.w };
  }
}
class DOMPoint extends DOMPointReadOnly {}
defineGlobal('DOMPointReadOnly', DOMPointReadOnly);
defineGlobal('DOMPoint', DOMPoint);

const MATRIX_NAMES = [
  'm11', 'm12', 'm13', 'm14', 'm21', 'm22', 'm23', 'm24',
  'm31', 'm32', 'm33', 'm34', 'm41', 'm42', 'm43', 'm44',
];
const IDENTITY = [1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1];
const kValues = Symbol('matrix');
const kIs2D = Symbol('is2D');
// The sixteen values a transform list's serialization stands for, if it is
// one a computed `transform` serializes as.
const parseTransform = (text) => {
  const source = String(text).trim();
  if (source === '' || source === 'none') return [IDENTITY.slice(), true];
  const match = /^(matrix|matrix3d)\(([^)]*)\)$/.exec(source);
  if (!match) throw new DOMException("'" + source + "' is not a transform matrix", 'SyntaxError');
  const numbers = match[2].split(',').map((part) => Number(part.trim()));
  if (numbers.some((number) => !Number.isFinite(number))) {
    throw new DOMException("'" + source + "' is not a transform matrix", 'SyntaxError');
  }
  if (match[1] === 'matrix' && numbers.length === 6) {
    const [a, b, c, d, e, f] = numbers;
    return [[a, b, 0, 0, c, d, 0, 0, 0, 0, 1, 0, e, f, 0, 1], true];
  }
  if (match[1] === 'matrix3d' && numbers.length === 16) return [numbers, false];
  throw new DOMException("'" + source + "' is not a transform matrix", 'SyntaxError');
};
const multiply = (left, right) => {
  const out = new Array(16).fill(0);
  for (let column = 0; column < 4; column++) {
    for (let row = 0; row < 4; row++) {
      let sum = 0;
      for (let k = 0; k < 4; k++) sum += left[k * 4 + row] * right[column * 4 + k];
      out[column * 4 + row] = sum;
    }
  }
  return out;
};
const invert = (m) => {
  const inv = new Array(16);
  inv[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15] + m[9] * m[7] * m[14] + m[13] * m[6] * m[11] - m[13] * m[7] * m[10];
  inv[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15] - m[8] * m[7] * m[14] - m[12] * m[6] * m[11] + m[12] * m[7] * m[10];
  inv[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15] + m[8] * m[7] * m[13] + m[12] * m[5] * m[11] - m[12] * m[7] * m[9];
  inv[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14] - m[8] * m[6] * m[13] - m[12] * m[5] * m[10] + m[12] * m[6] * m[9];
  inv[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15] - m[9] * m[3] * m[14] - m[13] * m[2] * m[11] + m[13] * m[3] * m[10];
  inv[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15] + m[8] * m[3] * m[14] + m[12] * m[2] * m[11] - m[12] * m[3] * m[10];
  inv[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15] - m[8] * m[3] * m[13] - m[12] * m[1] * m[11] + m[12] * m[3] * m[9];
  inv[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14] + m[8] * m[2] * m[13] + m[12] * m[1] * m[10] - m[12] * m[2] * m[9];
  inv[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15] + m[5] * m[3] * m[14] + m[13] * m[2] * m[7] - m[13] * m[3] * m[6];
  inv[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15] - m[4] * m[3] * m[14] - m[12] * m[2] * m[7] + m[12] * m[3] * m[6];
  inv[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15] + m[4] * m[3] * m[13] + m[12] * m[1] * m[7] - m[12] * m[3] * m[5];
  inv[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14] - m[4] * m[2] * m[13] - m[12] * m[1] * m[6] + m[12] * m[2] * m[5];
  inv[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11] - m[5] * m[3] * m[10] - m[9] * m[2] * m[7] + m[9] * m[3] * m[6];
  inv[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11] + m[4] * m[3] * m[10] + m[8] * m[2] * m[7] - m[8] * m[3] * m[6];
  inv[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11] - m[4] * m[3] * m[9] - m[8] * m[1] * m[7] + m[8] * m[3] * m[5];
  inv[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10] + m[4] * m[2] * m[9] + m[8] * m[1] * m[6] - m[8] * m[2] * m[5];
  const determinant = m[0] * inv[0] + m[1] * inv[4] + m[2] * inv[8] + m[3] * inv[12];
  if (determinant === 0) return null;
  return inv.map((value) => value / determinant);
};
class DOMMatrixReadOnly {
  constructor(init) {
    let values = IDENTITY.slice();
    let is2D = true;
    if (typeof init === 'string') {
      [values, is2D] = parseTransform(init);
    } else if (init !== undefined) {
      const list = Array.from(init, Number);
      if (list.length === 6) {
        const [a, b, c, d, e, f] = list;
        values = [a, b, 0, 0, c, d, 0, 0, 0, 0, 1, 0, e, f, 0, 1];
      } else if (list.length === 16) {
        values = list;
        is2D = false;
      } else {
        throw new TypeError('a matrix needs 6 or 16 values');
      }
    }
    this[kValues] = values;
    this[kIs2D] = is2D;
  }
  static fromMatrix(other = {}) {
    if (other instanceof DOMMatrixReadOnly) return new this(other[kValues].slice());
    const has3D = MATRIX_NAMES.some((name) => name in other) && other.is2D === false;
    if (has3D) return new this(MATRIX_NAMES.map((name, index) => other[name] ?? IDENTITY[index]));
    return new this([other.a ?? other.m11 ?? 1, other.b ?? other.m12 ?? 0, other.c ?? other.m21 ?? 0,
      other.d ?? other.m22 ?? 1, other.e ?? other.m41 ?? 0, other.f ?? other.m42 ?? 0]);
  }
  static fromFloat32Array(array) {
    return new this(Array.from(array));
  }
  static fromFloat64Array(array) {
    return new this(Array.from(array));
  }
  get is2D() {
    return this[kIs2D];
  }
  get isIdentity() {
    return this[kValues].every((value, index) => value === IDENTITY[index]);
  }
  get a() { return this[kValues][0]; }
  get b() { return this[kValues][1]; }
  get c() { return this[kValues][4]; }
  get d() { return this[kValues][5]; }
  get e() { return this[kValues][12]; }
  get f() { return this[kValues][13]; }
  multiply(other) {
    return new DOMMatrix(this[kValues]).multiplySelf(other);
  }
  translate(x = 0, y = 0, z = 0) {
    return new DOMMatrix(this[kValues]).translateSelf(x, y, z);
  }
  scale(x = 1, y = x, z = 1) {
    return new DOMMatrix(this[kValues]).scaleSelf(x, y, z);
  }
  inverse() {
    return new DOMMatrix(this[kValues]).invertSelf();
  }
  flipX() {
    return this.multiply(new DOMMatrix([-1, 0, 0, 1, 0, 0]));
  }
  flipY() {
    return this.multiply(new DOMMatrix([1, 0, 0, -1, 0, 0]));
  }
  transformPoint(point = {}) {
    const m = this[kValues];
    const x = point.x ?? 0;
    const y = point.y ?? 0;
    const z = point.z ?? 0;
    const w = point.w ?? 1;
    return new DOMPoint(
      m[0] * x + m[4] * y + m[8] * z + m[12] * w,
      m[1] * x + m[5] * y + m[9] * z + m[13] * w,
      m[2] * x + m[6] * y + m[10] * z + m[14] * w,
      m[3] * x + m[7] * y + m[11] * z + m[15] * w,
    );
  }
  toFloat32Array() {
    return new Float32Array(this[kValues]);
  }
  toFloat64Array() {
    return new Float64Array(this[kValues]);
  }
  toString() {
    const m = this[kValues];
    return this.is2D
      ? 'matrix(' + [m[0], m[1], m[4], m[5], m[12], m[13]].join(', ') + ')'
      : 'matrix3d(' + m.join(', ') + ')';
  }
  toJSON() {
    const out = { a: this.a, b: this.b, c: this.c, d: this.d, e: this.e, f: this.f, is2D: this.is2D };
    MATRIX_NAMES.forEach((name, index) => {
      out[name] = this[kValues][index];
    });
    return out;
  }
}
MATRIX_NAMES.forEach((name, index) => {
  Object.defineProperty(DOMMatrixReadOnly.prototype, name, {
    get() {
      return this[kValues][index];
    },
    enumerable: true,
    configurable: true,
  });
});
class DOMMatrix extends DOMMatrixReadOnly {
  multiplySelf(other) {
    const right = DOMMatrixReadOnly.fromMatrix(other);
    this[kValues] = multiply(this[kValues], right[kValues]);
    this[kIs2D] = this[kIs2D] && right.is2D;
    return this;
  }
  preMultiplySelf(other) {
    const left = DOMMatrixReadOnly.fromMatrix(other);
    this[kValues] = multiply(left[kValues], this[kValues]);
    this[kIs2D] = this[kIs2D] && left.is2D;
    return this;
  }
  translateSelf(x = 0, y = 0, z = 0) {
    this.multiplySelf(new DOMMatrix([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, Number(x), Number(y), Number(z), 1]));
    if (Number(z) === 0) this[kIs2D] = this[kIs2D] !== false && this[kValues].every((value, index) => [2, 3, 6, 7, 8, 9, 11, 14].includes(index) ? value === 0 : true);
    return this;
  }
  scaleSelf(x = 1, y = x, z = 1) {
    this[kValues] = multiply(this[kValues], [Number(x), 0, 0, 0, 0, Number(y), 0, 0, 0, 0, Number(z), 0, 0, 0, 0, 1]);
    if (Number(z) !== 1) this[kIs2D] = false;
    return this;
  }
  invertSelf() {
    const inverse = invert(this[kValues]);
    this[kValues] = inverse || new Array(16).fill(NaN);
    if (!inverse) this[kIs2D] = false;
    return this;
  }
  setMatrixValue(text) {
    [this[kValues], this[kIs2D]] = parseTransform(text);
    return this;
  }
}
MATRIX_NAMES.forEach((name, index) => {
  Object.defineProperty(DOMMatrix.prototype, name, {
    get() {
      return this[kValues][index];
    },
    set(value) {
      this[kValues][index] = Number(value);
      if (![0, 1, 4, 5, 12, 13, 10, 15].includes(index) && Number(value) !== 0) this[kIs2D] = false;
    },
    enumerable: true,
    configurable: true,
  });
});
for (const [name, index] of [['a', 0], ['b', 1], ['c', 4], ['d', 5], ['e', 12], ['f', 13]]) {
  Object.defineProperty(DOMMatrix.prototype, name, {
    get() {
      return this[kValues][index];
    },
    set(value) {
      this[kValues][index] = Number(value);
    },
    enumerable: true,
    configurable: true,
  });
}
defineGlobal('DOMMatrixReadOnly', DOMMatrixReadOnly);
defineGlobal('DOMMatrix', DOMMatrix);
defineGlobal('WebKitCSSMatrix', DOMMatrix);

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
    while (content.firstChild) callTree('appendChild', fragment, content.firstChild);
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
  if (!inPage(this)) return false;
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

// NonDocumentTypeChildNode (DOM §4.2.7) on character data: the element
// siblings of a text node or a comment. Lit's hydration finds the element a
// `<!--lit-node-->` marker stands for as the comment's next element sibling.
accessor(CharacterData.prototype, 'nextElementSibling', function () {
  let node = this.nextSibling;
  while (node && node.nodeType !== 1) node = node.nextSibling;
  return node;
});
accessor(CharacterData.prototype, 'previousElementSibling', function () {
  let node = this.previousSibling;
  while (node && node.nodeType !== 1) node = node.previousSibling;
  return node;
});

// ---------------------------------------------------------------- Messaging
// `window.postMessage` (HTML §9.4.3) to this window: there is no other
// browsing context here, so the only receiver is the page itself — which is
// how frameworks schedule work (a `message` they post to themselves) and how
// libraries test for the API. The message is a structured clone, delivered
// as a task, and dropped when `targetOrigin` names another origin.
defineGlobal('postMessage', function postMessage(message, options, transfer) {
  if (arguments.length < 1) throw new TypeError("Failed to execute 'postMessage' on 'Window': 1 argument required");
  let target = options !== null && typeof options === 'object' ? options.targetOrigin : options;
  target = target === undefined ? '/' : String(target);
  const own = location.origin;
  if (target !== '*' && target !== '/') {
    let parsed;
    try {
      parsed = new URL(target).origin;
    } catch (_) {
      throw new DOMException("Invalid target origin '" + target + "' in a call to 'postMessage'.", 'SyntaxError');
    }
    if (parsed !== own) return;
  }
  const data = structuredClone(message);
  setTimeout(() => {
    globalThis.dispatchEvent(new MessageEvent('message', { data, origin: own, source: globalThis }));
  }, 0);
});

// ---------------------------------------------------------------- Hit testing
// `elementsFromPoint` (CSSOM View §6.1): the elements whose boxes, as last
// drawn, contain the point, topmost first. Stacking order is approximated by
// tree order — a later element, or a descendant, is above — which is what
// a page with no `z-index` games sees.
method(Document.prototype, 'elementsFromPoint', function elementsFromPoint(x, y) {
  x = Number(x);
  y = Number(y);
  if (!Number.isFinite(x) || !Number.isFinite(y) || x < 0 || y < 0 || x > innerWidth || y > innerHeight) return [];
  const hits = [];
  for (const element of this.querySelectorAll('*')) {
    const rect = element.getBoundingClientRect();
    if (rect.width > 0 && rect.height > 0 && x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom) {
      hits.push(element);
    }
  }
  const root = this.documentElement;
  if (root && !hits.includes(root)) hits.unshift(root);
  return hits.reverse();
});
method(Document.prototype, 'elementFromPoint', function elementFromPoint(x, y) {
  return this.elementsFromPoint(x, y)[0] ?? null;
});

// ---------------------------------------------------------------- XMLSerializer
// DOM Parsing §3.2: markup for a node. HTML elements serialize as their HTML,
// with the XHTML namespace on the outermost one, as a browser's does.
const XHTML_NS = 'http://www.w3.org/1999/xhtml';
const escapeMarkup = (text) => String(text).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
class XMLSerializer {
  serializeToString(node) {
    if (!(node instanceof Node)) {
      throw new TypeError("Failed to execute 'serializeToString' on 'XMLSerializer': parameter 1 is not of type 'Node'.");
    }
    switch (node.nodeType) {
      case 1: {
        const markup = node.outerHTML;
        if (node.namespaceURI !== XHTML_NS || node.hasAttribute('xmlns')) return markup;
        return markup.replace(/^<([^\s>/]+)/, `<$1 xmlns="${XHTML_NS}"`);
      }
      case 3:
        return escapeMarkup(node.data);
      case 4:
        return '<![CDATA[' + node.data + ']]>';
      case 7:
        return '<?' + node.target + ' ' + node.data + '?>';
      case 8:
        return '<!--' + node.data + '-->';
      case 9: {
        const doctype = node.doctype ? this.serializeToString(node.doctype) : '';
        return doctype + (node.documentElement ? this.serializeToString(node.documentElement) : '');
      }
      case 10:
        return '<!DOCTYPE ' + node.name + (node.publicId ? ` PUBLIC "${node.publicId}"` : '') + (node.systemId ? ` "${node.systemId}"` : '') + '>';
      case 11:
        return Array.from(node.childNodes, (child) => this.serializeToString(child)).join('');
      default:
        return '';
    }
  }
}
defineGlobal('XMLSerializer', XMLSerializer);

// ---------------------------------------------------------------- FileReader
// File API §6: a Blob read as text, a data: URL, bytes or a binary string,
// with the reader's events in order.
class FileReader extends EventTarget {
  static EMPTY = 0;
  static LOADING = 1;
  static DONE = 2;
  #state = 0;
  #result = null;
  #error = null;
  #generation = 0;

  get readyState() {
    return this.#state;
  }
  get result() {
    return this.#result;
  }
  get error() {
    return this.#error;
  }
  #fire(type) {
    try {
      this.dispatchEvent(typeof ProgressEvent === 'function' ? new ProgressEvent(type) : new Event(type));
    } catch (error) {
      reportError(error);
    }
  }
  #read(blob, convert) {
    if (!(blob instanceof Blob)) throw new TypeError("Failed to execute on 'FileReader': parameter 1 is not of type 'Blob'.");
    if (this.#state === 1) throw new DOMException('The object is already busy reading Blobs.', 'InvalidStateError');
    this.#state = 1;
    this.#result = null;
    this.#error = null;
    const generation = ++this.#generation;
    setTimeout(() => this.#fire('loadstart'), 0);
    blob.arrayBuffer().then(
      (buffer) => {
        setTimeout(() => {
          if (generation !== this.#generation) return;
          this.#state = 2;
          this.#result = convert(buffer, blob);
          this.#fire('progress');
          this.#fire('load');
          this.#fire('loadend');
        }, 0);
      },
      (error) => {
        setTimeout(() => {
          if (generation !== this.#generation) return;
          this.#state = 2;
          this.#error = new DOMException(String(error && error.message || error), 'NotReadableError');
          this.#fire('error');
          this.#fire('loadend');
        }, 0);
      },
    );
  }
  readAsArrayBuffer(blob) {
    this.#read(blob, (buffer) => buffer);
  }
  readAsText(blob, encoding = 'utf-8') {
    this.#read(blob, (buffer) => new TextDecoder(encoding).decode(buffer));
  }
  readAsBinaryString(blob) {
    this.#read(blob, (buffer) => {
      let out = '';
      const bytes = new Uint8Array(buffer);
      for (let at = 0; at < bytes.length; at += 0x8000) out += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
      return out;
    });
  }
  readAsDataURL(blob) {
    this.#read(blob, (buffer, source) => {
      let binary = '';
      const bytes = new Uint8Array(buffer);
      for (let at = 0; at < bytes.length; at += 0x8000) binary += String.fromCharCode(...bytes.subarray(at, at + 0x8000));
      return 'data:' + (source.type || 'application/octet-stream') + ';base64,' + btoa(binary);
    });
  }
  abort() {
    if (this.#state !== 1) return;
    this.#generation++;
    this.#state = 2;
    this.#result = null;
    this.#fire('abort');
    this.#fire('loadend');
  }
}
Object.assign(FileReader.prototype, { EMPTY: 0, LOADING: 1, DONE: 2 });
for (const type of ['loadstart', 'progress', 'load', 'abort', 'error', 'loadend']) defineHandler(FileReader.prototype, type);
defineGlobal('FileReader', FileReader);

// WebSocket and EventSource are in `sockets.js`.

// ---------------------------------------------------------------- More event interfaces
// Event types a page constructs, or keeps the platform's `isTrusted` getter
// from (`new SecurityPolicyViolationEvent('…')`), with their init members
// read back as the interface's own accessors (`eventInterface`, in
// `uievents.js`). Nothing fires most of them yet.
eventInterface('DragEvent', MouseEvent, { dataTransfer: null });
eventInterface('ClipboardEvent', Event, { clipboardData: null });
eventInterface('FormDataEvent', Event, { formData: null });
eventInterface('BeforeUnloadEvent', Event, { returnValue: '' });
eventInterface('ToggleEvent', Event, { oldState: '', newState: '' });
eventInterface('TrackEvent', Event, { track: null });
eventInterface('GamepadEvent', Event, { gamepad: null });
eventInterface('DeviceOrientationEvent', Event, { alpha: null, beta: null, gamma: null, absolute: false });
eventInterface('SecurityPolicyViolationEvent', Event, {
  documentURI: '', referrer: '', blockedURI: '', violatedDirective: '', effectiveDirective: '',
  originalPolicy: '', sourceFile: '', sample: '', disposition: 'enforce', statusCode: 0,
  lineNumber: 0, columnNumber: 0,
});


// ---------------------------------------------------------------- Events
// HTML §12.2.1: what another document's change to storage tells this one.
// Fired at the window by `__otlyraStorageDeliver`, for every change another
// document of this origin made to `localStorage`.
class StorageEvent extends Event {
  #init;
  constructor(type, init = {}) {
    super(type, init);
    this.#init = {
      key: init.key ?? null,
      oldValue: init.oldValue ?? null,
      newValue: init.newValue ?? null,
      url: String(init.url ?? ''),
      storageArea: init.storageArea ?? null,
    };
  }
  get key() {
    return this.#init.key;
  }
  get oldValue() {
    return this.#init.oldValue;
  }
  get newValue() {
    return this.#init.newValue;
  }
  get url() {
    return this.#init.url;
  }
  get storageArea() {
    return this.#init.storageArea;
  }
}
defineGlobal('StorageEvent', StorageEvent);

const nativeStorageNextEvent = take(Document, '__storageNextEvent');
defineGlobal('__otlyraStorageDeliver', () => {
  let item;
  while ((item = nativeStorageNextEvent()) !== null && item !== undefined) {
    const [hasKey, key, hasOld, oldValue, hasNew, newValue, url] = item;
    window.dispatchEvent(new StorageEvent('storage', {
      key: hasKey === '1' ? key : null,
      oldValue: hasOld === '1' ? oldValue : null,
      newValue: hasNew === '1' ? newValue : null,
      url,
      storageArea: localStorage,
    }));
  }
});

// CSS Font Loading §2: a face a page builds in script. The faces a page
// names in its sheets load with the page; one made here is accepted and
// reports itself loaded, and text falls back to the faces the page has.
class FontFace {
  #family;
  #status = 'unloaded';
  #loaded;
  #resolve;

  constructor(family, source, descriptors = {}) {
    this.#family = String(family);
    this.#loaded = new Promise((resolve) => {
      this.#resolve = resolve;
    });
    this.style = String(descriptors.style ?? 'normal');
    this.weight = String(descriptors.weight ?? 'normal');
    this.stretch = String(descriptors.stretch ?? 'normal');
    this.unicodeRange = String(descriptors.unicodeRange ?? 'U+0-10FFFF');
    this.display = String(descriptors.display ?? 'auto');
    if (typeof source !== 'string') this.load();
  }
  get family() {
    return this.#family;
  }
  get status() {
    return this.#status;
  }
  get loaded() {
    return this.#loaded;
  }
  load() {
    if (this.#status === 'unloaded') {
      this.#status = 'loaded';
      this.#resolve(this);
    }
    return this.#loaded;
  }
}
defineGlobal('FontFace', FontFace);

// ---------------------------------------------------------------- Documents
// Documents script makes (DOM §4.5.1, HTML §8.5.1): each one a document of its
// own beside the page's, with its own nodes, which nothing draws, fetches
// pictures for or runs scripts in.
const XML_TYPES = new Set(['application/xml', 'text/xml', 'image/svg+xml', 'application/xhtml+xml']);
const SVG_NS = 'http://www.w3.org/2000/svg';
const implementations = new WeakMap();
// `document.implementation` (DOM §4.5.1): what makes documents, and doctypes
// for the document it belongs to.
class DOMImplementation {
  #document;
  constructor(owner) {
    if (!implementations.has(owner)) throw new TypeError('Illegal constructor');
    this.#document = owner;
  }
  createHTMLDocument(title) {
    const made = nativeCreateDocument('text/html');
    callTree('appendChild', made, nativeCreateDoctype.call(made, 'html', '', ''));
    const root = made.createElement('html');
    const head = made.createElement('head');
    callTree('appendChild', made, root);
    callTree('appendChild', root, head);
    if (title !== undefined) {
      const element = made.createElement('title');
      callTree('appendChild', element, made.createTextNode(String(title)));
      callTree('appendChild', head, element);
    }
    callTree('appendChild', root, made.createElement('body'));
    return made;
  }
  createDocument(namespace, qualifiedName, doctype = null) {
    const type =
      namespace === XHTML_NS ? 'application/xhtml+xml' : namespace === SVG_NS ? 'image/svg+xml' : 'application/xml';
    const made = nativeCreateDocument(type);
    const name = qualifiedName === null || qualifiedName === undefined ? '' : String(qualifiedName);
    const element = name === '' ? null : made.createElementNS(namespace, name);
    if (doctype) callTree('appendChild', made, doctype);
    if (element) callTree('appendChild', made, element);
    return made;
  }
  createDocumentType(name, publicId, systemId) {
    return nativeCreateDoctype.call(this.#document, String(name), String(publicId), String(systemId));
  }
  hasFeature() {
    return true;
  }
}
defineGlobal('DOMImplementation', DOMImplementation);
accessor(Document.prototype, 'implementation', function () {
  let made = implementations.get(this);
  if (!made) {
    implementations.set(this, null);
    made = new DOMImplementation(this);
    implementations.set(this, made);
  }
  return made;
});
class DOMParser {
  parseFromString(text, type) {
    const kind = String(type);
    if (kind !== 'text/html' && !XML_TYPES.has(kind)) {
      throw new TypeError(
        `Failed to execute 'parseFromString' on 'DOMParser': The provided value '${kind}' is not a valid enum value of type DOMParserSupportedType.`,
      );
    }
    const made = nativeCreateDocument(kind);
    nativeParseInto.call(made, String(text));
    return made;
  }
}
defineGlobal('DOMParser', DOMParser);

// `open`, `write` and `close` (HTML §8.4) on a document script made: what is
// written so far is that document's markup, parsed again with each write so
// it can be read before `close`. The page's own document cannot be written to
// yet — there is no insertion point in a parse that has finished, nor in one
// that is running — so there they do nothing rather than throw.
const written = new WeakMap();
method(Document.prototype, 'open', function open() {
  if (isMadeDocument(this)) {
    written.set(this, '');
    nativeParseInto.call(this, '');
  }
  return this;
});
method(Document.prototype, 'write', function write(...text) {
  if (!isMadeDocument(this)) return;
  const markup = (written.get(this) ?? '') + text.join('');
  written.set(this, markup);
  nativeParseInto.call(this, markup);
});
method(Document.prototype, 'writeln', function writeln(...text) {
  this.write(...text, '\n');
});
method(Document.prototype, 'close', function close() {
  written.delete(this);
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
  'ShadowRoot',
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
