// ---------------------------------------------------------------- Canvas 2D
// `getContext('2d')` (HTML §4.12.5.1). The bitmap and everything drawn into
// it are native: each call here is one call into the element's
// `__otlyraCanvas(op, numbers, text, source)`, which draws at once. What is
// kept here is what a page reads back — the attribute values, serialized
// the way a browser serializes them — so reading one costs no call.

const nativeCanvasOp = take(Element.prototype, '__otlyraCanvas');
const nativeCanvasPng = take(Element.prototype, '__otlyraCanvasPng');
const kContext = Symbol('context2d');
const kCanvas = Symbol('canvas');
const kState = Symbol('state');
const kStack = Symbol('stack');
const kGradient = Symbol('gradient');

const canvasOp = (canvas, op, numbers = [], text = null, source = null) =>
  nativeCanvasOp.call(canvas, op, new Float64Array(numbers), text, source);

const hex2 = (value) => Math.round(value).toString(16).padStart(2, '0');

// A colour serialized as a canvas serializes one: `#rrggbb` when opaque,
// `rgba(r, g, b, a)` otherwise.
const serializeColor = ([r, g, b, a]) =>
  a >= 1
    ? `#${hex2(r * 255)}${hex2(g * 255)}${hex2(b * 255)}`
    : `rgba(${Math.round(r * 255)}, ${Math.round(g * 255)}, ${Math.round(b * 255)}, ${+a.toFixed(3)})`;

class CanvasGradient {
  constructor(kind, points) {
    this[kGradient] = { kind, points, stops: [] };
  }
  addColorStop(offset, color) {
    offset = Number(offset);
    if (!(offset >= 0 && offset <= 1)) {
      throw new DOMException(`The provided value (${offset}) is outside the range (0.0, 1.0).`, 'IndexSizeError');
    }
    const rgba = canvasOp(document.documentElement, 'parseColor', [], String(color));
    if (!rgba) {
      throw new DOMException(`The value provided ('${color}') could not be parsed as a color.`, 'SyntaxError');
    }
    this[kGradient].stops.push([offset, rgba]);
  }
}

class CanvasPattern {
  setTransform() {}
}

class ImageData {
  constructor(first, second, third) {
    let data;
    let width;
    let height;
    if (first instanceof Uint8ClampedArray) {
      data = first;
      width = second >>> 0;
      if (!width || data.length % 4 !== 0 || (data.length / 4) % width !== 0) {
        throw new DOMException("The input data length is not a multiple of (4 * width).", 'IndexSizeError');
      }
      height = third === undefined ? data.length / 4 / width : third >>> 0;
    } else {
      width = first >>> 0;
      height = second >>> 0;
      if (!width || !height) throw new DOMException('The source width is 0.', 'IndexSizeError');
      data = new Uint8ClampedArray(width * height * 4);
    }
    Object.defineProperties(this, {
      width: { value: width, enumerable: true },
      height: { value: height, enumerable: true },
      data: { value: data, enumerable: true },
      colorSpace: { value: 'srgb', enumerable: true },
    });
  }
}

class TextMetrics {
  constructor(m) {
    const [width, left, right, ascent, descent, fontAscent, fontDescent] = m;
    Object.assign(this, {
      width,
      actualBoundingBoxLeft: left,
      actualBoundingBoxRight: right,
      actualBoundingBoxAscent: ascent,
      actualBoundingBoxDescent: descent,
      fontBoundingBoxAscent: fontAscent,
      fontBoundingBoxDescent: fontDescent,
      emHeightAscent: fontAscent,
      emHeightDescent: fontDescent,
      hangingBaseline: fontAscent * 0.8,
      alphabeticBaseline: 0,
      ideographicBaseline: -fontDescent,
    });
  }
}

const initialCanvasState = () => ({
  fillStyle: '#000000',
  strokeStyle: '#000000',
  lineWidth: 1,
  lineCap: 'butt',
  lineJoin: 'miter',
  miterLimit: 10,
  lineDash: [],
  lineDashOffset: 0,
  globalAlpha: 1,
  globalCompositeOperation: 'source-over',
  font: '10px sans-serif',
  textAlign: 'start',
  textBaseline: 'alphabetic',
  direction: 'inherit',
  letterSpacing: '0px',
  wordSpacing: '0px',
  fontKerning: 'auto',
  fontStretch: 'normal',
  fontVariantCaps: 'normal',
  textRendering: 'auto',
  imageSmoothingEnabled: true,
  imageSmoothingQuality: 'low',
  shadowBlur: 0,
  shadowColor: 'rgba(0, 0, 0, 0)',
  shadowOffsetX: 0,
  shadowOffsetY: 0,
  filter: 'none',
});

const FONT_SIZES = {
  'xx-small': 9, 'x-small': 10, small: 13, medium: 16, large: 18, 'x-large': 24, 'xx-large': 32, 'xxx-large': 48,
};

// The `font` shorthand, as far as a canvas needs it: style, weight, size and
// families. `null` for what is not a font.
const parseCanvasFont = (text) => {
  const value = String(text).trim();
  const match = /^((?:(?:italic|oblique|normal|small-caps|bold|bolder|lighter|[1-9]00|condensed|expanded|semi-condensed|semi-expanded|ultra-condensed|extra-condensed|ultra-expanded|extra-expanded)\s+)*)((?:\d*\.)?\d+(?:px|pt|em|rem|%)|xx-small|x-small|small|medium|large|x-large|xx-large|xxx-large)(?:\s*\/\s*[^\s]+)?\s+(.+)$/i.exec(value);
  if (!match) return null;
  let weight = 400;
  let italic = false;
  for (const word of match[1].trim().split(/\s+/).filter(Boolean)) {
    const lower = word.toLowerCase();
    if (lower === 'italic' || lower === 'oblique') italic = true;
    else if (lower === 'bold' || lower === 'bolder') weight = 700;
    else if (lower === 'lighter') weight = 300;
    else if (/^[1-9]00$/.test(lower)) weight = Number(lower);
  }
  const size = match[2].toLowerCase();
  let pixels;
  if (size in FONT_SIZES) pixels = FONT_SIZES[size];
  else {
    const number = parseFloat(size);
    if (size.endsWith('px')) pixels = number;
    else if (size.endsWith('pt')) pixels = (number * 4) / 3;
    else if (size.endsWith('%')) pixels = (number / 100) * 10;
    else pixels = number * 16;
  }
  const families = match[3]
    .split(',')
    .map((family) => family.trim().replace(/^["']|["']$/g, ''))
    .filter(Boolean);
  if (!families.length) return null;
  return { value, pixels, weight, italic, families };
};

// A Path2D's commands as numbers — a code, then its arguments — with any SVG
// path data it was made from beside them.
const PATH_CODES = {
  closePath: 0, moveTo: 1, lineTo: 2, bezierCurveTo: 3, quadraticCurveTo: 4,
  arc: 5, arcTo: 6, ellipse: 7, rect: 8, roundRect: 9, svg: 10,
};

const roundRectRadii = (radii) => {
  const list = radii === undefined ? [0] : Array.isArray(radii) ? radii : [radii];
  const values = list.map((radius) => (typeof radius === 'object' && radius ? Number(radius.x) || 0 : Number(radius) || 0));
  switch (values.length) {
    case 1: return [values[0], values[0], values[0], values[0]];
    case 2: return [values[0], values[1], values[0], values[1]];
    case 3: return [values[0], values[1], values[2], values[1]];
    default: return values.slice(0, 4);
  }
};

const encodePath = (path) => {
  const numbers = [];
  const svg = [];
  for (const [name, ...args] of path[kPathCommands]) {
    const code = PATH_CODES[name];
    if (code === undefined) continue;
    if (name === 'svg') {
      svg.push(String(args[0]));
      numbers.push(code, 0);
      continue;
    }
    let values;
    if (name === 'roundRect') values = [...args.slice(0, 4).map(Number), ...roundRectRadii(args[4])];
    else if (name === 'arc') values = [...args.slice(0, 5).map(Number), args[5] ? 1 : 0];
    else if (name === 'ellipse') values = [...args.slice(0, 7).map(Number), args[7] ? 1 : 0];
    else values = args.map(Number);
    numbers.push(code, values.length, ...values);
  }
  return [numbers, svg.join('\0')];
};

const fillRule = (rule) => (rule === 'evenodd' ? 1 : 0);

class CanvasRenderingContext2D {
  constructor(canvas) {
    this[kCanvas] = canvas;
    this[kState] = initialCanvasState();
    this[kStack] = [];
  }
  get canvas() {
    return this[kCanvas];
  }
  getContextAttributes() {
    return { alpha: true, colorSpace: 'srgb', desynchronized: false, willReadFrequently: false };
  }
  #op(op, numbers, text, source) {
    return canvasOp(this[kCanvas], op, numbers, text, source);
  }

  save() {
    this[kStack].push({ ...this[kState] });
    this.#op('save');
  }
  restore() {
    if (!this[kStack].length) return;
    this[kState] = this[kStack].pop();
    this.#op('restore');
  }
  reset() {
    this[kState] = initialCanvasState();
    this[kStack] = [];
    this.#op('reset');
  }
  isContextLost() {
    return false;
  }

  scale(x, y) { this.#op('transform', [x, 0, 0, y, 0, 0]); }
  rotate(angle) {
    const cos = Math.cos(angle);
    const sin = Math.sin(angle);
    this.#op('transform', [cos, sin, -sin, cos, 0, 0]);
  }
  translate(x, y) { this.#op('transform', [1, 0, 0, 1, x, y]); }
  transform(a, b, c, d, e, f) { this.#op('transform', [a, b, c, d, e, f]); }
  setTransform(a, b, c, d, e, f) {
    if (a === undefined || typeof a === 'object') {
      const m = a ?? {};
      this.#op('setTransform', [m.a ?? m.m11 ?? 1, m.b ?? m.m12 ?? 0, m.c ?? m.m21 ?? 0, m.d ?? m.m22 ?? 1, m.e ?? m.m41 ?? 0, m.f ?? m.m42 ?? 0]);
      return;
    }
    this.#op('setTransform', [a, b, c, d, e, f]);
  }
  resetTransform() { this.#op('setTransform', [1, 0, 0, 1, 0, 0]); }
  getTransform() {
    const [a, b, c, d, e, f] = this.#op('getTransform');
    return new DOMMatrix([a, b, c, d, e, f]);
  }

  createLinearGradient(x0, y0, x1, y1) {
    return new CanvasGradient(0, [x0, y0, x1, y1].map(Number));
  }
  createRadialGradient(x0, y0, r0, x1, y1, r1) {
    if (r0 < 0 || r1 < 0) throw new DOMException('The radius provided is negative.', 'IndexSizeError');
    return new CanvasGradient(1, [x0, y0, r0, x1, y1, r1].map(Number));
  }
  createConicGradient(angle, x, y) {
    return new CanvasGradient(2, [angle, x, y].map(Number));
  }
  createPattern() {
    return new CanvasPattern();
  }
  createImageData(width, height) {
    if (width instanceof ImageData) return new ImageData(width.width, width.height);
    return new ImageData(Math.abs(width >> 0) || 1, Math.abs(height >> 0) || 1);
  }

  beginPath() { this.#op('beginPath'); }
  closePath() { this.#op('closePath'); }
  moveTo(x, y) { this.#op('moveTo', [x, y]); }
  lineTo(x, y) { this.#op('lineTo', [x, y]); }
  quadraticCurveTo(cx, cy, x, y) { this.#op('quadraticCurveTo', [cx, cy, x, y]); }
  bezierCurveTo(c1x, c1y, c2x, c2y, x, y) { this.#op('bezierCurveTo', [c1x, c1y, c2x, c2y, x, y]); }
  arc(x, y, radius, start, end, ccw = false) {
    if (radius < 0) throw new DOMException(`The radius provided (${radius}) is negative.`, 'IndexSizeError');
    this.#op('arc', [x, y, radius, start, end, ccw ? 1 : 0]);
  }
  arcTo(x1, y1, x2, y2, radius) {
    if (radius < 0) throw new DOMException(`The radius provided (${radius}) is negative.`, 'IndexSizeError');
    this.#op('arcTo', [x1, y1, x2, y2, radius]);
  }
  ellipse(x, y, rx, ry, rotation, start, end, ccw = false) {
    if (rx < 0 || ry < 0) throw new DOMException('The radius provided is negative.', 'IndexSizeError');
    this.#op('ellipse', [x, y, rx, ry, rotation, start, end, ccw ? 1 : 0]);
  }
  rect(x, y, w, h) { this.#op('rect', [x, y, w, h]); }
  roundRect(x, y, w, h, radii) { this.#op('roundRect', [x, y, w, h, ...roundRectRadii(radii)]); }

  fill(path, rule) {
    if (path instanceof Path2D) {
      const [numbers, svg] = encodePath(path);
      this.#op('fillPath', [fillRule(rule), ...numbers], svg);
    } else this.#op('fill', [fillRule(path)]);
  }
  stroke(path) {
    if (path instanceof Path2D) {
      const [numbers, svg] = encodePath(path);
      this.#op('strokePath', [0, ...numbers], svg);
    } else this.#op('stroke');
  }
  clip(path, rule) {
    if (path instanceof Path2D) {
      const [numbers, svg] = encodePath(path);
      this.#op('clipPath', [fillRule(rule), ...numbers], svg);
    } else this.#op('clip', [fillRule(path)]);
  }
  isPointInPath(...args) {
    if (args[0] instanceof Path2D) {
      const [path, x, y, rule] = args;
      const [numbers, svg] = encodePath(path);
      return !!this.#op('isPointInPathOf', [x, y, fillRule(rule), ...numbers], svg)?.[0];
    }
    const [x, y, rule] = args;
    return !!this.#op('isPointInPath', [x, y, fillRule(rule)])?.[0];
  }
  isPointInStroke() {
    return false;
  }
  drawFocusIfNeeded() {}
  scrollPathIntoView() {}

  fillRect(x, y, w, h) { this.#op('fillRect', [x, y, w, h]); }
  strokeRect(x, y, w, h) { this.#op('strokeRect', [x, y, w, h]); }
  clearRect(x, y, w, h) { this.#op('clearRect', [x, y, w, h]); }

  fillText(text, x, y, maxWidth) {
    this.#op('fillText', maxWidth === undefined ? [x, y] : [x, y, maxWidth], String(text));
  }
  strokeText(text, x, y, maxWidth) {
    this.#op('strokeText', maxWidth === undefined ? [x, y] : [x, y, maxWidth], String(text));
  }
  measureText(text) {
    return new TextMetrics(this.#op('measureText', [], String(text)) ?? [0, 0, 0, 0, 0, 0, 0]);
  }

  drawImage(image, ...args) {
    let width;
    let height;
    if (image instanceof HTMLImageElement) {
      if (!image.complete || !image.naturalWidth) return;
      width = image.naturalWidth;
      height = image.naturalHeight;
    } else if (image instanceof HTMLCanvasElement) {
      width = image.width;
      height = image.height;
      if (!width || !height) {
        throw new DOMException('The image argument is a canvas element with a width or height of 0.', 'InvalidStateError');
      }
    } else if (image && typeof image === 'object' && 'width' in image && 'height' in image) {
      // An ImageBitmap, a video or an SVG image: nothing to draw from yet.
      return;
    } else {
      throw new TypeError("Failed to execute 'drawImage' on 'CanvasRenderingContext2D': The provided value is not of a supported type.");
    }
    let from;
    let to;
    if (args.length <= 2) {
      from = [0, 0, width, height];
      to = [args[0], args[1], width, height];
    } else if (args.length <= 4) {
      from = [0, 0, width, height];
      to = args.slice(0, 4);
    } else {
      from = args.slice(0, 4);
      to = args.slice(4, 8);
    }
    this.#op('drawImage', [...from, ...to].map(Number), null, image);
  }

  getImageData(x, y, w, h) {
    w = w >> 0;
    h = h >> 0;
    if (!w || !h) throw new DOMException('The source width or height is 0.', 'IndexSizeError');
    if (w < 0) { x += w; w = -w; }
    if (h < 0) { y += h; h = -h; }
    const pixels = this.#op('getImageData', [x >> 0, y >> 0, w, h]) ?? [];
    const data = new Uint8ClampedArray(w * h * 4);
    data.set(pixels);
    return new ImageData(data, w, h);
  }
  putImageData(imageData, dx, dy, dirtyX, dirtyY, dirtyWidth, dirtyHeight) {
    if (!(imageData instanceof ImageData)) {
      throw new TypeError("Failed to execute 'putImageData' on 'CanvasRenderingContext2D': parameter 1 is not of type 'ImageData'.");
    }
    let { width, height, data } = imageData;
    let x = dx >> 0;
    let y = dy >> 0;
    if (dirtyX !== undefined) {
      const left = Math.max(0, dirtyX >> 0);
      const top = Math.max(0, dirtyY >> 0);
      const right = Math.min(width, left + (dirtyWidth >> 0));
      const bottom = Math.min(height, top + (dirtyHeight >> 0));
      if (right <= left || bottom <= top) return;
      const part = new Uint8ClampedArray((right - left) * (bottom - top) * 4);
      for (let row = top; row < bottom; row++) {
        part.set(data.subarray((row * width + left) * 4, (row * width + right) * 4), (row - top) * (right - left) * 4);
      }
      data = part;
      x += left;
      y += top;
      width = right - left;
      height = bottom - top;
    }
    this.#op('putImageData', [width, height, x, y, ...data]);
  }

  setLineDash(segments) {
    const values = [...segments].map(Number);
    if (values.some((value) => !Number.isFinite(value) || value < 0)) return;
    this[kState].lineDash = values.length % 2 ? [...values, ...values] : values;
    this.#op('setLineDash', values);
  }
  getLineDash() {
    return [...this[kState].lineDash];
  }
}

const paintSetter = (name, op) =>
  function (value) {
    if (value instanceof CanvasGradient) {
      const { kind, points, stops } = value[kGradient];
      if (kind === 2) return;
      this[kState][name] = value;
      const numbers = [kind, ...points];
      for (const [offset, rgba] of stops) numbers.push(offset, ...rgba);
      canvasOp(this[kCanvas], op + 'Gradient', numbers);
      return;
    }
    if (value instanceof CanvasPattern) {
      this[kState][name] = value;
      return;
    }
    const rgba = canvasOp(this[kCanvas], op, [], String(value));
    if (rgba) this[kState][name] = serializeColor(rgba);
  };

accessor(CanvasRenderingContext2D.prototype, 'fillStyle', function () {
  return this[kState].fillStyle;
}, paintSetter('fillStyle', 'fillStyle'));
accessor(CanvasRenderingContext2D.prototype, 'strokeStyle', function () {
  return this[kState].strokeStyle;
}, paintSetter('strokeStyle', 'strokeStyle'));

const numberSetting = (name, valid) => {
  accessor(CanvasRenderingContext2D.prototype, name, function () {
    return this[kState][name];
  }, function (value) {
    value = Number(value);
    if (!Number.isFinite(value) || !valid(value)) return;
    this[kState][name] = value;
    canvasOp(this[kCanvas], name, [value]);
  });
};
numberSetting('lineWidth', (value) => value > 0);
numberSetting('miterLimit', (value) => value > 0);
numberSetting('globalAlpha', (value) => value >= 0 && value <= 1);
numberSetting('lineDashOffset', () => true);

const keywordSetting = (name, keywords) => {
  accessor(CanvasRenderingContext2D.prototype, name, function () {
    return this[kState][name];
  }, function (value) {
    value = String(value);
    if (keywords && !keywords.includes(value)) return;
    if (!keywords && !canvasOp(this[kCanvas], name, [], value)) return;
    this[kState][name] = value;
    if (keywords) canvasOp(this[kCanvas], name, [], value);
  });
};
keywordSetting('lineCap', ['butt', 'round', 'square']);
keywordSetting('lineJoin', ['round', 'bevel', 'miter']);
keywordSetting('textAlign', ['start', 'end', 'left', 'right', 'center']);
keywordSetting('textBaseline', ['top', 'hanging', 'middle', 'alphabetic', 'ideographic', 'bottom']);
keywordSetting('globalCompositeOperation', null);

accessor(CanvasRenderingContext2D.prototype, 'font', function () {
  return this[kState].font;
}, function (value) {
  const font = parseCanvasFont(value);
  if (!font) return;
  this[kState].font = font.value;
  canvasOp(this[kCanvas], 'font', [font.pixels, font.weight, font.italic ? 1 : 0], font.families.join('\n'));
});

accessor(CanvasRenderingContext2D.prototype, 'imageSmoothingEnabled', function () {
  return this[kState].imageSmoothingEnabled;
}, function (value) {
  this[kState].imageSmoothingEnabled = !!value;
  canvasOp(this[kCanvas], 'imageSmoothingEnabled', [value ? 1 : 0]);
});

// Kept and read back, and not drawn with yet.
for (const name of [
  'shadowBlur', 'shadowColor', 'shadowOffsetX', 'shadowOffsetY', 'filter', 'direction', 'letterSpacing',
  'wordSpacing', 'fontKerning', 'fontStretch', 'fontVariantCaps', 'textRendering', 'imageSmoothingQuality',
]) {
  accessor(CanvasRenderingContext2D.prototype, name, function () {
    return this[kState][name];
  }, function (value) {
    this[kState][name] = typeof this[kState][name] === 'number' ? Number(value) : String(value);
  });
}

// Setting either dimension, even to what it was, clears the bitmap.
for (const [name, fallback] of [['width', 300], ['height', 150]]) {
  const { get } = Object.getOwnPropertyDescriptor(HTMLCanvasElement.prototype, name);
  accessor(HTMLCanvasElement.prototype, name, get, function (value) {
    value = Number(value);
    this.setAttribute(name, String(Number.isFinite(value) && value >= 0 && value <= 2147483647 ? Math.floor(value) : fallback));
    if (this[kContext]) {
      this[kContext][kState] = initialCanvasState();
      this[kContext][kStack] = [];
      canvasOp(this, 'resize');
    }
  });
}

method(HTMLCanvasElement.prototype, 'getContext', function getContext(kind) {
  if (String(kind) !== '2d') return null;
  if (!this[kContext]) {
    this[kContext] = new CanvasRenderingContext2D(this);
    canvasOp(this, 'resize');
  }
  return this[kContext];
});
method(HTMLCanvasElement.prototype, 'toDataURL', function toDataURL() {
  if (!this.width || !this.height) return 'data:,';
  const png = nativeCanvasPng.call(this);
  return png ? `data:image/png;base64,${png}` : 'data:,';
});
method(HTMLCanvasElement.prototype, 'toBlob', function toBlob(callback) {
  if (typeof callback !== 'function') {
    throw new TypeError("Failed to execute 'toBlob' on 'HTMLCanvasElement': The callback provided as parameter 1 is not a function.");
  }
  const url = this.toDataURL();
  Promise.resolve().then(() => {
    if (url === 'data:,') return callback(null);
    const bytes = Uint8Array.from(atob(url.slice(url.indexOf(',') + 1)), (c) => c.charCodeAt(0));
    callback(new Blob([bytes], { type: 'image/png' }));
  });
});
method(HTMLCanvasElement.prototype, 'transferControlToOffscreen', function transferControlToOffscreen() {
  throw new DOMException('OffscreenCanvas is not supported.', 'NotSupportedError');
});

defineGlobal('CanvasRenderingContext2D', CanvasRenderingContext2D);
defineGlobal('CanvasGradient', CanvasGradient);
defineGlobal('CanvasPattern', CanvasPattern);
defineGlobal('ImageData', ImageData);
defineGlobal('TextMetrics', TextMetrics);
