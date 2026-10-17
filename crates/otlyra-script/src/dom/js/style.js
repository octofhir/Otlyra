// `element.style`, which is a live view of one attribute the way `classList`
// is a live view of another. A page reads it to feel out what the browser
// supports — React asks `"animation" in element.style` before it decides what
// to listen for — and writes it to move things about.
//
// The property list is fixed rather than open-ended, because `in` has to
// answer honestly: a style object that claimed every name would tell a page
// we support things we do not.
const STYLE_PROPERTIES = [
  'accentColor', 'alignContent', 'alignItems', 'alignSelf', 'all', 'animation',
  'animationDelay', 'animationDirection', 'animationDuration', 'animationFillMode',
  'animationIterationCount', 'animationName', 'animationPlayState',
  'animationTimingFunction', 'appearance', 'aspectRatio', 'backfaceVisibility',
  'background', 'backgroundAttachment', 'backgroundBlendMode', 'backgroundClip',
  'backgroundColor', 'backgroundImage', 'backgroundOrigin', 'backgroundPosition',
  'backgroundPositionX', 'backgroundPositionY', 'backgroundRepeat', 'backgroundSize',
  'blockSize', 'border', 'borderBlock', 'borderBlockEnd', 'borderBlockStart',
  'borderBottom', 'borderBottomColor', 'borderBottomLeftRadius', 'borderBottomRightRadius',
  'borderBottomStyle', 'borderBottomWidth', 'borderCollapse', 'borderColor',
  'borderImage', 'borderInline', 'borderInlineEnd', 'borderInlineStart', 'borderLeft',
  'borderLeftColor', 'borderLeftStyle', 'borderLeftWidth', 'borderRadius',
  'borderRight', 'borderRightColor', 'borderRightStyle', 'borderRightWidth',
  'borderSpacing', 'borderStyle', 'borderTop', 'borderTopColor', 'borderTopLeftRadius',
  'borderTopRightRadius', 'borderTopStyle', 'borderTopWidth', 'borderWidth', 'bottom',
  'boxShadow', 'boxSizing', 'breakAfter', 'breakBefore', 'breakInside', 'captionSide',
  'caretColor', 'clear', 'clip', 'clipPath', 'color', 'colorScheme', 'columnCount',
  'columnGap', 'columnRule', 'columnWidth', 'columns', 'contain', 'container',
  'containerName', 'containerType', 'content', 'contentVisibility', 'counterIncrement', 'counterReset',
  'cursor', 'direction', 'display', 'emptyCells', 'fill', 'filter', 'flex', 'flexBasis',
  'flexDirection', 'flexFlow', 'flexGrow', 'flexShrink', 'flexWrap', 'float', 'font',
  'fontFamily', 'fontFeatureSettings', 'fontKerning', 'fontSize', 'fontStretch',
  'fontStyle', 'fontVariant', 'fontVariantNumeric', 'fontWeight', 'gap', 'grid',
  'gridArea', 'gridAutoColumns', 'gridAutoFlow', 'gridAutoRows', 'gridColumn',
  'gridColumnEnd', 'gridColumnStart', 'gridRow', 'gridRowEnd', 'gridRowStart',
  'gridTemplate', 'gridTemplateAreas', 'gridTemplateColumns', 'gridTemplateRows',
  'height', 'hyphens', 'imageRendering', 'inlineSize', 'inset', 'insetBlock',
  'insetInline', 'isolation', 'justifyContent', 'justifyItems', 'justifySelf', 'left',
  'letterSpacing', 'lineBreak', 'lineHeight', 'listStyle', 'listStyleImage',
  'listStylePosition', 'listStyleType', 'margin', 'marginBlock', 'marginBlockEnd',
  'marginBlockStart', 'marginBottom', 'marginInline', 'marginInlineEnd',
  'marginInlineStart', 'marginLeft', 'marginRight', 'marginTop', 'mask', 'maskImage',
  'maxBlockSize', 'maxHeight', 'maxInlineSize', 'maxWidth', 'minBlockSize', 'minHeight',
  'minInlineSize', 'minWidth', 'mixBlendMode', 'objectFit', 'objectPosition', 'opacity',
  'order', 'outline', 'outlineColor', 'outlineOffset', 'outlineStyle', 'outlineWidth',
  'overflow', 'overflowAnchor', 'overflowWrap', 'overflowX', 'overflowY',
  'overscrollBehavior', 'padding', 'paddingBlock', 'paddingBlockEnd',
  'paddingBlockStart', 'paddingBottom', 'paddingInline', 'paddingInlineEnd',
  'paddingInlineStart', 'paddingLeft', 'paddingRight', 'paddingTop', 'perspective',
  'perspectiveOrigin', 'placeContent', 'placeItems', 'placeSelf', 'pointerEvents',
  'position', 'quotes', 'resize', 'right', 'rotate', 'rowGap', 'scale', 'scrollBehavior',
  'scrollMargin', 'scrollPadding', 'scrollSnapAlign', 'scrollSnapType',
  'scrollbarColor', 'scrollbarGutter', 'scrollbarWidth', 'stroke', 'strokeWidth',
  'tabSize', 'tableLayout', 'textAlign', 'textAlignLast', 'textDecoration',
  'textDecorationColor', 'textDecorationLine', 'textDecorationStyle',
  'textDecorationThickness', 'textIndent', 'textOverflow', 'textRendering',
  'textShadow', 'textTransform', 'textUnderlineOffset', 'textWrap', 'top', 'touchAction',
  'transform', 'transformOrigin', 'transformStyle', 'transition', 'transitionDelay',
  'transitionDuration', 'transitionProperty', 'transitionTimingFunction', 'translate',
  'unicodeBidi', 'userSelect', 'verticalAlign', 'visibility', 'whiteSpace', 'width',
  'willChange', 'wordBreak', 'wordSpacing', 'wordWrap', 'writingMode', 'zIndex',
];

// A property's name as the declaration keeps it: lowercase, except a custom
// property's, which is case-sensitive (CSS Variables §2).
const propertyName = (name) => {
  const trimmed = String(name).trim();
  return trimmed.startsWith('--') ? trimmed : trimmed.toLowerCase();
};

const hyphenate = (name) => name.replace(/[A-Z]/g, (letter) => '-' + letter.toLowerCase());

class CSSStyleDeclaration {
  #owner;

  constructor(owner) {
    this.#owner = owner;
  }

  #declarations() {
    const map = new Map();
    const text = this.#owner.getAttribute('style') || '';
    for (const part of text.split(';')) {
      const at = part.indexOf(':');
      if (at === -1) continue;
      const name = propertyName(part.slice(0, at));
      const value = part.slice(at + 1).trim();
      if (name && value) map.set(name, value);
    }
    return map;
  }

  #write(map) {
    const text = [...map.entries()].map(([name, value]) => name + ': ' + value).join('; ');
    this.#owner.setAttribute('style', text);
  }

  get cssText() {
    return this.#owner.getAttribute('style') || '';
  }

  set cssText(next) {
    this.#owner.setAttribute('style', String(next));
  }

  get length() {
    return this.#declarations().size;
  }

  item(index) {
    return [...this.#declarations().keys()][index] ?? '';
  }

  getPropertyValue(name) {
    return this.#declarations().get(propertyName(name)) ?? '';
  }

  setProperty(name, value) {
    const map = this.#declarations();
    const property = propertyName(name);
    if (value === null || value === undefined || value === '') map.delete(property);
    else map.set(property, String(value));
    this.#write(map);
  }

  removeProperty(name) {
    const map = this.#declarations();
    const property = propertyName(name);
    const had = map.get(property) ?? '';
    map.delete(property);
    this.#write(map);
    return had;
  }
}

for (const property of STYLE_PROPERTIES) {
  Object.defineProperty(CSSStyleDeclaration.prototype, property, {
    get() {
      return this.getPropertyValue(hyphenate(property));
    },
    set(value) {
      this.setProperty(hyphenate(property), value);
    },
    enumerable: true,
    configurable: true,
  });
  // The hyphenated spelling answers to the same declaration: a page may write
  // either, and `in` is asked with both.
  const dashed = hyphenate(property);
  if (dashed !== property && !(dashed in CSSStyleDeclaration.prototype)) {
    Object.defineProperty(CSSStyleDeclaration.prototype, dashed, {
      get() {
        return this.getPropertyValue(dashed);
      },
      set(value) {
        this.setProperty(dashed, value);
      },
      enumerable: false,
      configurable: true,
    });
  }
}

// `cssFloat` is `float` under the name the object model could use when `float`
// was a reserved word.
Object.defineProperty(CSSStyleDeclaration.prototype, 'cssFloat', {
  get() {
    return this.getPropertyValue('float');
  },
  set(value) {
    this.setProperty('float', value);
  },
  enumerable: true,
  configurable: true,
});
method(CSSStyleDeclaration.prototype, 'getPropertyPriority', function getPropertyPriority() {
  return '';
});
readonly(CSSStyleDeclaration.prototype, 'parentRule', null);
defineGlobal('CSSStyleDeclaration', CSSStyleDeclaration);

// One declaration object per element, so `el.style === el.style`.
const kStyle = Symbol('style');
for (const Interface of [HTMLElement, SVGElement, MathMLElement]) {
  accessor(
    Interface.prototype,
    'style',
    function () {
      let style = this[kStyle];
      if (!style) {
        style = new CSSStyleDeclaration(this);
        Object.defineProperty(this, kStyle, { value: style });
      }
      return style;
    },
    function (value) {
      this.setAttribute('style', String(value));
    },
  );
}

// `getComputedStyle` (CSSOM §9): a live, read-only declaration whose values
// are the element's resolved values on the last frame the browser styled.
// Like geometry, a change script made in this turn shows on the next frame,
// not on this read.
//
// What the style system cannot say, layout does: `width` and `height` of an
// element that was drawn are its used size (CSSOM §9's resolved value for
// them), from the box the last frame drew. An element that was not styled
// then — detached, or met before the page's first frame — answers from its
// own `style` attribute, and with the empty string beyond that.
const sideNames = { width: ['left', 'right'], height: ['top', 'bottom'] };
// Properties the style system parses only behind a switch that is off, and
// so never computes, answered with their initial values — what they compute
// to on every element until it is on. Container queries are off (see
// `cascade::enable_features`); a page that walks up looking for its query
// container reads these and splits the name.
const UNCOMPUTED_INITIAL = new Map([
  ['container', 'none'],
  ['container-name', 'none'],
  ['container-type', 'normal'],
]);
const pixels = (value) => {
  const number = parseFloat(value);
  return Number.isFinite(number) ? number : 0;
};
class ComputedStyleDeclaration extends CSSStyleDeclaration {
  #element;

  constructor(element) {
    super(element);
    this.#element = element;
  }

  #computed(property) {
    return this.#element.__otlyraComputedStyle(property);
  }

  get cssText() {
    return '';
  }

  set cssText(_) {
    throw new DOMException('a computed style cannot be changed', 'NoModificationAllowedError');
  }

  get length() {
    return STYLE_PROPERTIES.length;
  }

  item(index) {
    const property = STYLE_PROPERTIES[index];
    return property === undefined ? '' : hyphenate(property);
  }

  getPropertyValue(name) {
    const property = propertyName(name);
    const value = this.#computed(property);
    const sides = sideNames[property];
    if (sides && value !== null && this.#computed('display') !== 'none') {
      const rect = this.#element.__otlyraClientRect();
      if (rect) {
        let size = property === 'width' ? rect[2] - rect[0] : rect[3] - rect[1];
        if (this.#computed('box-sizing') !== 'border-box') {
          for (const side of sides) {
            size -= pixels(this.#computed('padding-' + side));
            size -= pixels(this.#computed('border-' + side + '-width'));
          }
        }
        return Math.max(0, size) + 'px';
      }
    }
    if (value !== null && value !== undefined) return value;
    if (UNCOMPUTED_INITIAL.has(property)) return UNCOMPUTED_INITIAL.get(property);
    return super.getPropertyValue(property);
  }

  setProperty() {
    throw new DOMException('a computed style cannot be changed', 'NoModificationAllowedError');
  }

  removeProperty() {
    throw new DOMException('a computed style cannot be changed', 'NoModificationAllowedError');
  }
}

defineGlobal('getComputedStyle', function getComputedStyle(element, pseudoElement) {
  if (!(element instanceof Element)) {
    throw new TypeError("getComputedStyle's argument is not an element");
  }
  return new ComputedStyleDeclaration(element);
});
