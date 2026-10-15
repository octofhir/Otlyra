// Content attributes as IDL attributes (HTML §2.6.1 "reflecting content
// attributes in IDL attributes"): `a.href` is the `href` attribute, resolved;
// `input.disabled` is whether `disabled` is present; `td.colSpan` is a number
// clamped to what the attribute may hold. A page sets these as properties
// and expects the attribute to change — the attribute is what style,
// selectors, forms and the renderer read.

// HTML §2.3.4.1 "rules for parsing integers": leading white space, a sign,
// digits; anything else is an error, which is `null` here.
const parseInteger = (value) => {
  if (value === null) return null;
  const match = /^[\t\n\f\r ]*([+-]?)(\d+)/.exec(value);
  if (!match) return null;
  const parsed = Number(match[2]);
  return match[1] === '-' ? -parsed : parsed;
};

// HTML §2.3.4.3 "rules for parsing floating-point number values", leniently.
const parseDouble = (value) => {
  if (value === null) return null;
  const parsed = Number.parseFloat(value);
  return Number.isFinite(parsed) ? parsed : null;
};

const reflectString = (Interface, property, attribute = property.toLowerCase()) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      return this.getAttribute(attribute) ?? '';
    },
    function (value) {
      this.setAttribute(attribute, String(value));
    },
  );

// A string that is `null` when the attribute is absent (`crossOrigin`).
const reflectNullable = (Interface, property, attribute = property.toLowerCase()) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      return this.getAttribute(attribute);
    },
    function (value) {
      if (value === null) this.removeAttribute(attribute);
      else this.setAttribute(attribute, String(value));
    },
  );

const reflectBoolean = (Interface, property, attribute = property.toLowerCase()) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      return this.hasAttribute(attribute);
    },
    function (value) {
      this.toggleAttribute(attribute, Boolean(value));
    },
  );

// A URL, resolved against the document's base when read.
const reflectUrl = (Interface, property, attribute = property.toLowerCase()) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      const value = this.getAttribute(attribute);
      if (value === null) return '';
      try {
        return new URL(value.trim(), documentBase()).href;
      } catch (_) {
        return value;
      }
    },
    function (value) {
      this.setAttribute(attribute, String(value));
    },
  );

// `long`: the attribute parsed as an integer, or the default.
const reflectLong = (Interface, property, attribute, fallback, minimum = -2147483648) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      const parsed = parseInteger(this.getAttribute(attribute));
      return parsed === null || parsed < minimum || parsed > 2147483647 ? fallback : parsed;
    },
    function (value) {
      const number = Math.trunc(Number(value)) || 0;
      if (number < minimum) {
        throw new DOMException(property + ' may not be ' + number, 'IndexSizeError');
      }
      this.setAttribute(attribute, String(number));
    },
  );

// `unsigned long`, optionally clamped to a range (`colSpan`, `rowSpan`).
const reflectUnsigned = (Interface, property, attribute, fallback, lowest = 0, highest = 2147483647) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      const parsed = parseInteger(this.getAttribute(attribute));
      if (parsed === null || parsed < lowest) return fallback;
      return Math.min(parsed, highest);
    },
    function (value) {
      const number = Number(value) >>> 0;
      this.setAttribute(attribute, String(number > 2147483647 ? fallback : number));
    },
  );

const reflectDouble = (Interface, property, attribute, fallback) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      const parsed = parseDouble(this.getAttribute(attribute));
      return parsed === null ? fallback : parsed;
    },
    function (value) {
      const number = Number(value);
      if (!Number.isFinite(number)) throw new TypeError(property + ' must be a finite number');
      this.setAttribute(attribute, String(number));
    },
  );

// An enumerated attribute: one of `keywords`, lowercased, or the default for a
// missing value and for an invalid one.
const reflectEnum = (Interface, property, attribute, keywords, missing, invalid = missing) =>
  accessor(
    Interface.prototype,
    property,
    function () {
      const value = this.getAttribute(attribute);
      if (value === null) return missing;
      const lowered = value.toLowerCase();
      return keywords.indexOf(lowered) !== -1 ? lowered : invalid;
    },
    function (value) {
      this.setAttribute(attribute, String(value));
    },
  );

// --------------------------------------------------------- HTMLElement

reflectString(HTMLElement, 'title');
reflectString(HTMLElement, 'lang');
reflectEnum(HTMLElement, 'dir', 'dir', ['ltr', 'rtl', 'auto'], '');
reflectBoolean(HTMLElement, 'hidden');
reflectString(HTMLElement, 'accessKey', 'accesskey');
reflectBoolean(HTMLElement, 'autofocus');
reflectBoolean(HTMLElement, 'inert');
reflectString(HTMLElement, 'nonce');
reflectString(HTMLElement, 'autocapitalize');
reflectString(HTMLElement, 'enterKeyHint', 'enterkeyhint');
reflectString(HTMLElement, 'inputMode', 'inputmode');
reflectNullable(HTMLElement, 'popover');
for (const Interface of [SVGElement, MathMLElement]) {
  reflectString(Interface, 'nonce');
  reflectBoolean(Interface, 'autofocus');
}

// What may be focused with Tab, absent a `tabindex` (HTML §6.6.3).
const FOCUSABLE = /^(a|area|button|input|select|textarea|iframe|summary)$/;
const focusableByDefault = (element) => {
  const name = element.localName;
  if (!FOCUSABLE.test(name)) return element.getAttribute('contenteditable') !== null;
  if (name === 'a' || name === 'area') return element.hasAttribute('href');
  return !element.hasAttribute('disabled');
};
for (const Interface of [HTMLElement, SVGElement, MathMLElement]) {
  accessor(
    Interface.prototype,
    'tabIndex',
    function () {
      const parsed = parseInteger(this.getAttribute('tabindex'));
      if (parsed !== null) return parsed;
      return focusableByDefault(this) ? 0 : -1;
    },
    function (value) {
      this.setAttribute('tabindex', String(Math.trunc(Number(value)) || 0));
    },
  );
}

accessor(
  HTMLElement.prototype,
  'draggable',
  function () {
    const value = (this.getAttribute('draggable') || '').toLowerCase();
    if (value === 'true') return true;
    if (value === 'false') return false;
    return this.localName === 'img' || ((this.localName === 'a' || this.localName === 'area') && this.hasAttribute('href'));
  },
  function (value) {
    this.setAttribute('draggable', value ? 'true' : 'false');
  },
);
accessor(
  HTMLElement.prototype,
  'spellcheck',
  function () {
    return (this.getAttribute('spellcheck') || '').toLowerCase() !== 'false';
  },
  function (value) {
    this.setAttribute('spellcheck', value ? 'true' : 'false');
  },
);
accessor(
  HTMLElement.prototype,
  'translate',
  function () {
    for (let node = this; node && node.nodeType === 1; node = node.parentElement) {
      const value = node.getAttribute('translate');
      if (value !== null) return value.toLowerCase() !== 'no';
    }
    return true;
  },
  function (value) {
    this.setAttribute('translate', value ? 'yes' : 'no');
  },
);
accessor(
  HTMLElement.prototype,
  'contentEditable',
  function () {
    const value = this.getAttribute('contenteditable');
    if (value === null) return 'inherit';
    const lowered = value.toLowerCase();
    if (lowered === '' || lowered === 'true') return 'true';
    if (lowered === 'false' || lowered === 'plaintext-only') return lowered;
    return 'inherit';
  },
  function (value) {
    const lowered = String(value).toLowerCase();
    if (lowered === 'inherit') this.removeAttribute('contenteditable');
    else if (lowered === 'true' || lowered === 'false' || lowered === 'plaintext-only') {
      this.setAttribute('contenteditable', lowered);
    } else {
      throw new DOMException("'" + value + "' is not a contentEditable value", 'SyntaxError');
    }
  },
);
accessor(HTMLElement.prototype, 'isContentEditable', function () {
  for (let node = this; node && node.nodeType === 1; node = node.parentElement) {
    const value = node.getAttribute('contenteditable');
    if (value !== null) return value.toLowerCase() !== 'false';
  }
  return false;
});

// `innerText` is what a page reads when it wants text a reader could have
// seen: collapsed white space, no `<script>` and no `display: none`. That is a
// question about layout, and script has no layout to ask yet — so this is
// `textContent`, which is the same answer for every element that is not
// rendered and a white-space-noisy one for those that are.
for (const name of ['innerText', 'outerText']) {
  accessor(
    HTMLElement.prototype,
    name,
    function () {
      return this.textContent;
    },
    function (value) {
      this.textContent = value;
    },
  );
}

// No focus model a page can see yet, and no event the browser would send with
// them: these keep a page that calls them running.
for (const Interface of [HTMLElement, SVGElement]) {
  method(Interface.prototype, 'focus', function focus() {
    this.__otlyraFocus();
  });
  method(Interface.prototype, 'blur', function blur() {
    this.__otlyraBlur();
  });
}

// `click()` sends the event a click sends (HTML §6.4.3); what a click on the
// element would *do* is the browser's, when it sends one itself.
method(HTMLElement.prototype, 'click', function click() {
  const Click = typeof MouseEvent === 'function' ? MouseEvent : Event;
  this.dispatchEvent(new Click('click', { bubbles: true, cancelable: true, composed: true }));
});

// ------------------------------------------------- links and addresses

// HTML §4.6.3 HTMLHyperlinkElementUtils: the parts of `href`, read and set.
const URL_PARTS = ['protocol', 'username', 'password', 'host', 'hostname', 'port', 'pathname', 'search', 'hash'];
for (const Interface of [HTMLAnchorElement, HTMLAreaElement]) {
  reflectUrl(Interface, 'href');
  reflectString(Interface, 'target');
  reflectString(Interface, 'download');
  reflectString(Interface, 'ping');
  reflectString(Interface, 'rel');
  reflectString(Interface, 'referrerPolicy', 'referrerpolicy');
  tokenList(Interface, 'relList', 'rel', ['noreferrer', 'noopener', 'opener']);
  for (const part of URL_PARTS) {
    accessor(
      Interface.prototype,
      part,
      function () {
        try {
          return new URL(this.href)[part];
        } catch (_) {
          return '';
        }
      },
      function (value) {
        try {
          const url = new URL(this.href);
          url[part] = value;
          this.setAttribute('href', url.href);
        } catch (_) {
          // An `href` that is not a URL has no parts to set (HTML §4.6.3).
        }
      },
    );
  }
  accessor(Interface.prototype, 'origin', function () {
    try {
      return new URL(this.href).origin;
    } catch (_) {
      return '';
    }
  });
  method(Interface.prototype, 'toString', function toString() {
    return this.href;
  });
}
reflectString(HTMLAnchorElement, 'hreflang');
reflectString(HTMLAnchorElement, 'type');
reflectString(HTMLAnchorElement, 'name');
accessor(
  HTMLAnchorElement.prototype,
  'text',
  function () {
    return this.textContent;
  },
  function (value) {
    this.textContent = value;
  },
);
reflectString(HTMLAreaElement, 'alt');
reflectString(HTMLAreaElement, 'coords');
reflectString(HTMLAreaElement, 'shape');

reflectUrl(HTMLLinkElement, 'href');
reflectString(HTMLLinkElement, 'rel');
tokenList(HTMLLinkElement, 'relList', 'rel', [
  'alternate', 'dns-prefetch', 'icon', 'manifest', 'modulepreload', 'next', 'pingback',
  'preconnect', 'prefetch', 'preload', 'search', 'stylesheet',
]);
reflectString(HTMLLinkElement, 'media');
reflectString(HTMLLinkElement, 'hreflang');
reflectString(HTMLLinkElement, 'type');
reflectString(HTMLLinkElement, 'as');
reflectNullable(HTMLLinkElement, 'crossOrigin', 'crossorigin');
reflectString(HTMLLinkElement, 'integrity');
reflectString(HTMLLinkElement, 'referrerPolicy', 'referrerpolicy');
reflectString(HTMLLinkElement, 'imageSrcset', 'imagesrcset');
reflectString(HTMLLinkElement, 'imageSizes', 'imagesizes');
reflectString(HTMLLinkElement, 'fetchPriority', 'fetchpriority');
reflectBoolean(HTMLLinkElement, 'disabled');
tokenList(HTMLLinkElement, 'sizes', 'sizes');

// A base's `href` resolves against the document's own address, not against
// itself (HTML §4.2.3).
accessor(
  HTMLBaseElement.prototype,
  'href',
  function () {
    const value = this.getAttribute('href');
    if (value === null) return nativeUrl();
    try {
      return new URL(value, nativeUrl()).href;
    } catch (_) {
      return value;
    }
  },
  function (value) {
    this.setAttribute('href', String(value));
  },
);
reflectString(HTMLBaseElement, 'target');

// ------------------------------------------------- scripts and styles

reflectUrl(HTMLScriptElement, 'src');
reflectString(HTMLScriptElement, 'type');
reflectBoolean(HTMLScriptElement, 'noModule', 'nomodule');
reflectBoolean(HTMLScriptElement, 'defer');
reflectNullable(HTMLScriptElement, 'crossOrigin', 'crossorigin');
reflectString(HTMLScriptElement, 'integrity');
reflectString(HTMLScriptElement, 'referrerPolicy', 'referrerpolicy');
reflectString(HTMLScriptElement, 'charset');
reflectString(HTMLScriptElement, 'fetchPriority', 'fetchpriority');
// `async` as the attribute. The "force async" flag a script made by script
// starts with is the loader's to keep, and arrives with it.
reflectBoolean(HTMLScriptElement, 'async');
for (const Interface of [HTMLScriptElement, HTMLTitleElement]) {
  accessor(
    Interface.prototype,
    'text',
    function () {
      let text = '';
      for (let child = this.firstChild; child; child = child.nextSibling) {
        if (child.nodeType === 3) text += child.data;
      }
      return text;
    },
    function (value) {
      this.textContent = value;
    },
  );
}
HTMLScriptElement.supports = function supports(type) {
  return type === 'classic' || type === 'module';
};

reflectString(HTMLStyleElement, 'media');
reflectString(HTMLStyleElement, 'type');
reflectBoolean(HTMLStyleElement, 'disabled');
define(HTMLStyleElement.prototype, 'sheet', null);
define(HTMLLinkElement.prototype, 'sheet', null);

reflectString(HTMLMetaElement, 'name');
reflectString(HTMLMetaElement, 'httpEquiv', 'http-equiv');
reflectString(HTMLMetaElement, 'content');
reflectString(HTMLMetaElement, 'media');
reflectString(HTMLHtmlElement, 'version');

// ------------------------------------------------------------- embedded

reflectUrl(HTMLImageElement, 'src');
// A picture outside the document — `new Image()` preloading one — is fetched
// all the same, and its element hears `load` or `error`.
{
  const src = Object.getOwnPropertyDescriptor(HTMLImageElement.prototype, 'src');
  accessor(HTMLImageElement.prototype, 'src', src.get, function (value) {
    src.set.call(this, value);
    if (!this.isConnected) this.__otlyraRequestImage();
  });
}
reflectString(HTMLImageElement, 'srcset');
reflectString(HTMLImageElement, 'sizes');
reflectString(HTMLImageElement, 'alt');
reflectString(HTMLImageElement, 'useMap', 'usemap');
reflectBoolean(HTMLImageElement, 'isMap', 'ismap');
reflectNullable(HTMLImageElement, 'crossOrigin', 'crossorigin');
reflectString(HTMLImageElement, 'referrerPolicy', 'referrerpolicy');
reflectEnum(HTMLImageElement, 'decoding', 'decoding', ['sync', 'async', 'auto'], 'auto');
reflectEnum(HTMLImageElement, 'loading', 'loading', ['lazy', 'eager'], 'eager');
reflectString(HTMLImageElement, 'fetchPriority', 'fetchpriority');
reflectUnsigned(HTMLImageElement, 'width', 'width', 0);
reflectUnsigned(HTMLImageElement, 'height', 'height', 0);
// What became of the picture: its pixel size once it has arrived, and
// whether there is nothing left to wait for (arrived, failed, or no `src`).
accessor(HTMLImageElement.prototype, 'naturalWidth', function () {
  return this.__otlyraImageState()[0];
});
accessor(HTMLImageElement.prototype, 'naturalHeight', function () {
  return this.__otlyraImageState()[1];
});
accessor(HTMLImageElement.prototype, 'complete', function () {
  return this.__otlyraImageState()[2] === 1;
});
accessor(HTMLImageElement.prototype, 'currentSrc', function () {
  return this.src;
});
method(HTMLImageElement.prototype, 'decode', function decode() {
  return Promise.resolve();
});

reflectUrl(HTMLIFrameElement, 'src');
reflectString(HTMLIFrameElement, 'srcdoc');
reflectString(HTMLIFrameElement, 'name');
reflectString(HTMLIFrameElement, 'allow');
reflectBoolean(HTMLIFrameElement, 'allowFullscreen', 'allowfullscreen');
reflectString(HTMLIFrameElement, 'width');
reflectString(HTMLIFrameElement, 'height');
reflectString(HTMLIFrameElement, 'referrerPolicy', 'referrerpolicy');
reflectEnum(HTMLIFrameElement, 'loading', 'loading', ['lazy', 'eager'], 'eager');
tokenList(HTMLIFrameElement, 'sandbox', 'sandbox', [
  'allow-downloads', 'allow-forms', 'allow-modals', 'allow-orientation-lock',
  'allow-pointer-lock', 'allow-popups', 'allow-popups-to-escape-sandbox',
  'allow-presentation', 'allow-same-origin', 'allow-scripts', 'allow-top-navigation',
  'allow-top-navigation-by-user-activation', 'allow-top-navigation-to-custom-protocols',
]);
define(HTMLIFrameElement.prototype, 'contentWindow', null);
define(HTMLIFrameElement.prototype, 'contentDocument', null);

reflectUrl(HTMLEmbedElement, 'src');
reflectString(HTMLEmbedElement, 'type');
reflectString(HTMLEmbedElement, 'width');
reflectString(HTMLEmbedElement, 'height');
reflectUrl(HTMLObjectElement, 'data');
reflectString(HTMLObjectElement, 'type');
reflectString(HTMLObjectElement, 'name');
reflectString(HTMLObjectElement, 'width');
reflectString(HTMLObjectElement, 'height');
define(HTMLObjectElement.prototype, 'contentDocument', null);
define(HTMLObjectElement.prototype, 'contentWindow', null);

reflectUrl(HTMLSourceElement, 'src');
reflectString(HTMLSourceElement, 'type');
reflectString(HTMLSourceElement, 'srcset');
reflectString(HTMLSourceElement, 'sizes');
reflectString(HTMLSourceElement, 'media');
reflectUnsigned(HTMLSourceElement, 'width', 'width', 0);
reflectUnsigned(HTMLSourceElement, 'height', 'height', 0);
reflectString(HTMLTrackElement, 'kind');
reflectUrl(HTMLTrackElement, 'src');
reflectString(HTMLTrackElement, 'srclang');
reflectString(HTMLTrackElement, 'label');
reflectBoolean(HTMLTrackElement, 'default');
reflectString(HTMLMapElement, 'name');
accessor(HTMLMapElement.prototype, 'areas', function () {
  return asCollection([...this.querySelectorAll('area')]);
});

reflectUnsigned(HTMLCanvasElement, 'width', 'width', 300);
reflectUnsigned(HTMLCanvasElement, 'height', 'height', 150);
// No canvas backend yet: `getContext` answers as a browser that does not offer
// the context asked for does, and a page falls back to what it draws without.
method(HTMLCanvasElement.prototype, 'getContext', function getContext() {
  return null;
});
method(HTMLCanvasElement.prototype, 'toDataURL', function toDataURL() {
  return 'data:,';
});
method(HTMLCanvasElement.prototype, 'toBlob', function toBlob(callback) {
  if (typeof callback === 'function') Promise.resolve().then(() => callback(null));
});

// Media that cannot play: the members a page reads before it decides what to
// do, answering as an element with nothing loaded does (HTML §4.8.11).
reflectUrl(HTMLMediaElement, 'src');
reflectNullable(HTMLMediaElement, 'crossOrigin', 'crossorigin');
reflectEnum(HTMLMediaElement, 'preload', 'preload', ['none', 'metadata', 'auto'], 'auto', 'auto');
reflectBoolean(HTMLMediaElement, 'autoplay');
reflectBoolean(HTMLMediaElement, 'loop');
reflectBoolean(HTMLMediaElement, 'controls');
reflectBoolean(HTMLMediaElement, 'defaultMuted', 'muted');
const MEDIA_STATE = {
  currentSrc: '',
  currentTime: 0,
  duration: NaN,
  paused: true,
  ended: false,
  seeking: false,
  volume: 1,
  playbackRate: 1,
  defaultPlaybackRate: 1,
  readyState: 0,
  networkState: 0,
  error: null,
  buffered: null,
  played: null,
  seekable: null,
  textTracks: null,
};
for (const name of Object.keys(MEDIA_STATE)) {
  Object.defineProperty(HTMLMediaElement.prototype, name, {
    value: MEDIA_STATE[name],
    writable: true,
    enumerable: true,
    configurable: true,
  });
}
accessor(
  HTMLMediaElement.prototype,
  'muted',
  function () {
    return this.hasAttribute('muted');
  },
  function (value) {
    this.toggleAttribute('muted', Boolean(value));
  },
);
method(HTMLMediaElement.prototype, 'play', function play() {
  return Promise.reject(new DOMException('media playback is not supported', 'NotSupportedError'));
});
method(HTMLMediaElement.prototype, 'pause', function pause() {});
method(HTMLMediaElement.prototype, 'load', function load() {});
method(HTMLMediaElement.prototype, 'canPlayType', function canPlayType() {
  return '';
});
reflectUnsigned(HTMLVideoElement, 'width', 'width', 0);
reflectUnsigned(HTMLVideoElement, 'height', 'height', 0);
reflectUrl(HTMLVideoElement, 'poster');
reflectBoolean(HTMLVideoElement, 'playsInline', 'playsinline');
define(HTMLVideoElement.prototype, 'videoWidth', 0);
define(HTMLVideoElement.prototype, 'videoHeight', 0);

// ---------------------------------------------------------------- forms

// The form an element belongs to: its `form` attribute's, or its nearest
// form ancestor's (HTML §4.10.17.3, without re-association on mutation).
const formOwner = (element) => {
  const named = element.getAttribute('form');
  if (named !== null) {
    const found = (element.ownerDocument || document).getElementById(named);
    return found && found.localName === 'form' ? found : null;
  }
  return element.closest('form');
};

const LISTED = 'button, fieldset, input, object, output, select, textarea';

const FORM_CONTROLS = [
  HTMLButtonElement,
  HTMLFieldSetElement,
  HTMLInputElement,
  HTMLObjectElement,
  HTMLOutputElement,
  HTMLSelectElement,
  HTMLTextAreaElement,
  HTMLLabelElement,
  HTMLLegendElement,
  HTMLOptionElement,
];
for (const Interface of FORM_CONTROLS) {
  accessor(Interface.prototype, 'form', function () {
    if (this.localName === 'label') {
      const control = this.control;
      return control ? control.form : null;
    }
    if (this.localName === 'legend') {
      const fieldset = this.parentElement;
      return fieldset && fieldset.localName === 'fieldset' ? fieldset.form : null;
    }
    return formOwner(this);
  });
}

// Constraint validation (HTML §4.10.20), as a form that is always valid:
// nothing is checked yet, so nothing is refused.
const VALIDITY = {
  valueMissing: false,
  typeMismatch: false,
  patternMismatch: false,
  tooLong: false,
  tooShort: false,
  rangeUnderflow: false,
  rangeOverflow: false,
  stepMismatch: false,
  badInput: false,
  customError: false,
  valid: true,
};
for (const Interface of [HTMLButtonElement, HTMLFieldSetElement, HTMLInputElement, HTMLObjectElement, HTMLOutputElement, HTMLSelectElement, HTMLTextAreaElement]) {
  accessor(Interface.prototype, 'validity', function () {
    return Object.assign({}, VALIDITY);
  });
  accessor(Interface.prototype, 'validationMessage', function () {
    return '';
  });
  accessor(Interface.prototype, 'willValidate', function () {
    return !this.hasAttribute('disabled');
  });
  method(Interface.prototype, 'checkValidity', function checkValidity() {
    return true;
  });
  method(Interface.prototype, 'reportValidity', function reportValidity() {
    return true;
  });
  method(Interface.prototype, 'setCustomValidity', function setCustomValidity() {});
  accessor(Interface.prototype, 'labels', function () {
    const labels = [...(this.ownerDocument || document).querySelectorAll('label')].filter(
      (label) => label.control === this,
    );
    return asNodeList(labels);
  });
}

reflectString(HTMLFormElement, 'acceptCharset', 'accept-charset');
accessor(
  HTMLFormElement.prototype,
  'action',
  function () {
    const value = this.getAttribute('action');
    if (value === null || value === '') return nativeUrl();
    try {
      return new URL(value, documentBase()).href;
    } catch (_) {
      return value;
    }
  },
  function (value) {
    this.setAttribute('action', String(value));
  },
);
reflectEnum(HTMLFormElement, 'autocomplete', 'autocomplete', ['on', 'off'], 'on');
const ENCTYPES = ['application/x-www-form-urlencoded', 'multipart/form-data', 'text/plain'];
reflectEnum(HTMLFormElement, 'enctype', 'enctype', ENCTYPES, ENCTYPES[0]);
reflectEnum(HTMLFormElement, 'encoding', 'enctype', ENCTYPES, ENCTYPES[0]);
reflectEnum(HTMLFormElement, 'method', 'method', ['get', 'post', 'dialog'], 'get');
reflectString(HTMLFormElement, 'name');
reflectBoolean(HTMLFormElement, 'noValidate', 'novalidate');
reflectString(HTMLFormElement, 'target');
reflectString(HTMLFormElement, 'rel');
tokenList(HTMLFormElement, 'relList', 'rel', ['noreferrer', 'noopener', 'opener']);
accessor(HTMLFormElement.prototype, 'elements', function () {
  const listed = [...(this.ownerDocument || document).querySelectorAll(LISTED)].filter(
    (element) => element.form === this && !(element.localName === 'input' && element.type === 'image'),
  );
  return asCollection(listed);
});
accessor(HTMLFormElement.prototype, 'length', function () {
  return this.elements.length;
});
method(HTMLFormElement.prototype, 'requestSubmit', function requestSubmit() {
  this.submit();
});
method(HTMLFormElement.prototype, 'reset', function reset() {});
method(HTMLFormElement.prototype, 'checkValidity', function checkValidity() {
  return true;
});
method(HTMLFormElement.prototype, 'reportValidity', function reportValidity() {
  return true;
});

// HTML §4.10.5's `type` keywords; anything else is a text field.
const INPUT_TYPES = [
  'hidden', 'text', 'search', 'tel', 'url', 'email', 'password', 'date', 'month', 'week',
  'time', 'datetime-local', 'number', 'range', 'color', 'checkbox', 'radio', 'file',
  'submit', 'image', 'reset', 'button',
];
reflectEnum(HTMLInputElement, 'type', 'type', INPUT_TYPES, 'text', 'text');
for (const [property, attribute] of [
  ['accept', 'accept'], ['alt', 'alt'], ['autocomplete', 'autocomplete'],
  ['dirName', 'dirname'], ['max', 'max'], ['min', 'min'], ['name', 'name'],
  ['pattern', 'pattern'], ['placeholder', 'placeholder'], ['step', 'step'],
  ['formTarget', 'formtarget'], ['formEnctype', 'formenctype'], ['formMethod', 'formmethod'],
]) {
  reflectString(HTMLInputElement, property, attribute);
}
reflectUrl(HTMLInputElement, 'src');
reflectBoolean(HTMLInputElement, 'defaultChecked', 'checked');
reflectBoolean(HTMLInputElement, 'disabled');
reflectBoolean(HTMLInputElement, 'multiple');
reflectBoolean(HTMLInputElement, 'readOnly', 'readonly');
reflectBoolean(HTMLInputElement, 'required');
reflectBoolean(HTMLInputElement, 'formNoValidate', 'formnovalidate');
reflectLong(HTMLInputElement, 'maxLength', 'maxlength', -1, 0);
reflectLong(HTMLInputElement, 'minLength', 'minlength', -1, 0);
reflectUnsigned(HTMLInputElement, 'size', 'size', 20, 1);
reflectUnsigned(HTMLInputElement, 'width', 'width', 0);
reflectUnsigned(HTMLInputElement, 'height', 'height', 0);
reflectString(HTMLInputElement, 'defaultValue', 'value');
// What the reader typed and ticked, which the document's form state holds
// beside the attributes; `defaultValue`/`defaultChecked` are the attributes.
accessor(
  HTMLInputElement.prototype,
  'value',
  function () {
    return this.__otlyraControlValue();
  },
  function (value) {
    this.__otlyraSetControlValue(value === null ? '' : String(value));
  },
);
accessor(
  HTMLInputElement.prototype,
  'checked',
  function () {
    return this.__otlyraControlChecked();
  },
  function (value) {
    this.__otlyraSetControlChecked(Boolean(value));
  },
);
const kIndeterminate = Symbol('indeterminate');
accessor(
  HTMLInputElement.prototype,
  'indeterminate',
  function () {
    return Boolean(this[kIndeterminate]);
  },
  function (value) {
    this[kIndeterminate] = Boolean(value);
  },
);
define(HTMLInputElement.prototype, 'files', null);
define(HTMLInputElement.prototype, 'selectionStart', null);
define(HTMLInputElement.prototype, 'selectionEnd', null);
define(HTMLInputElement.prototype, 'selectionDirection', null);
method(HTMLInputElement.prototype, 'select', function select() {});
method(HTMLInputElement.prototype, 'setSelectionRange', function setSelectionRange() {});
method(HTMLInputElement.prototype, 'setRangeText', function setRangeText() {});
method(HTMLInputElement.prototype, 'showPicker', function showPicker() {});
method(HTMLInputElement.prototype, 'stepUp', function stepUp() {});
method(HTMLInputElement.prototype, 'stepDown', function stepDown() {});
accessor(HTMLInputElement.prototype, 'valueAsNumber', function () {
  const parsed = Number.parseFloat(this.value);
  return Number.isFinite(parsed) ? parsed : NaN;
});

reflectBoolean(HTMLButtonElement, 'disabled');
reflectString(HTMLButtonElement, 'name');
reflectString(HTMLButtonElement, 'value');
reflectEnum(HTMLButtonElement, 'type', 'type', ['submit', 'reset', 'button'], 'submit', 'submit');
reflectString(HTMLButtonElement, 'formTarget', 'formtarget');
reflectBoolean(HTMLButtonElement, 'formNoValidate', 'formnovalidate');
for (const Interface of [HTMLButtonElement, HTMLInputElement]) {
  accessor(
    Interface.prototype,
    'formAction',
    function () {
      const value = this.getAttribute('formaction');
      if (value === null || value === '') return nativeUrl();
      try {
        return new URL(value, documentBase()).href;
      } catch (_) {
        return value;
      }
    },
    function (value) {
      this.setAttribute('formaction', String(value));
    },
  );
}

reflectString(HTMLSelectElement, 'autocomplete');
reflectBoolean(HTMLSelectElement, 'disabled');
reflectBoolean(HTMLSelectElement, 'multiple');
reflectString(HTMLSelectElement, 'name');
reflectBoolean(HTMLSelectElement, 'required');
reflectUnsigned(HTMLSelectElement, 'size', 'size', 0);
accessor(HTMLSelectElement.prototype, 'type', function () {
  return this.multiple ? 'select-multiple' : 'select-one';
});
accessor(HTMLSelectElement.prototype, 'options', function () {
  return asCollection([...this.querySelectorAll('option')]);
});
accessor(HTMLSelectElement.prototype, 'length', function () {
  return this.options.length;
});
accessor(HTMLSelectElement.prototype, 'selectedOptions', function () {
  return asCollection([...this.options].filter((option) => option.selected));
});
accessor(
  HTMLSelectElement.prototype,
  'selectedIndex',
  function () {
    const options = [...this.options];
    const chosen = options.findIndex((option) => option.hasAttribute('selected'));
    if (chosen !== -1) return chosen;
    return !this.multiple && options.length ? 0 : -1;
  },
  function (value) {
    const options = [...this.options];
    options.forEach((option, index) => option.toggleAttribute('selected', index === Number(value)));
  },
);
accessor(
  HTMLSelectElement.prototype,
  'value',
  function () {
    const option = this.options[this.selectedIndex];
    return option ? option.value : '';
  },
  function (value) {
    const wanted = String(value);
    const options = [...this.options];
    const index = options.findIndex((option) => option.value === wanted);
    this.selectedIndex = index;
  },
);
method(HTMLSelectElement.prototype, 'item', function item(index) {
  return this.options.item(index);
});
method(HTMLSelectElement.prototype, 'namedItem', function namedItem(name) {
  return this.options.namedItem(name);
});
method(HTMLSelectElement.prototype, 'add', function add(element, before) {
  const reference = typeof before === 'number' ? this.options[before] || null : before || null;
  if (reference && reference.parentNode) reference.parentNode.insertBefore(element, reference);
  else this.appendChild(element);
});
// `select.remove()` is ChildNode's; `select.remove(index)` removes an option.
const childRemove = Element.prototype.remove;
method(HTMLSelectElement.prototype, 'remove', function remove(index) {
  if (arguments.length === 0) {
    childRemove.call(this);
    return;
  }
  const option = this.options[Number(index)];
  if (option) option.remove();
});

reflectBoolean(HTMLOptionElement, 'disabled');
reflectBoolean(HTMLOptionElement, 'defaultSelected', 'selected');
reflectBoolean(HTMLOptionElement, 'selected', 'selected');
// The option's text with its white space stripped and collapsed
// (HTML §4.10.10).
const optionText = (option) => option.textContent.split(/[\t\n\f\r ]+/).filter(Boolean).join(' ');
accessor(
  HTMLOptionElement.prototype,
  'label',
  function () {
    const value = this.getAttribute('label');
    return value === null ? optionText(this) : value;
  },
  function (value) {
    this.setAttribute('label', String(value));
  },
);
accessor(
  HTMLOptionElement.prototype,
  'value',
  function () {
    const value = this.getAttribute('value');
    return value === null ? optionText(this) : value;
  },
  function (value) {
    this.setAttribute('value', String(value));
  },
);
accessor(
  HTMLOptionElement.prototype,
  'text',
  function () {
    return optionText(this);
  },
  function (value) {
    this.textContent = value;
  },
);
accessor(HTMLOptionElement.prototype, 'index', function () {
  const select = this.closest('select');
  return select ? [...select.options].indexOf(this) : 0;
});
reflectBoolean(HTMLOptGroupElement, 'disabled');
reflectString(HTMLOptGroupElement, 'label');

reflectString(HTMLTextAreaElement, 'autocomplete');
reflectUnsigned(HTMLTextAreaElement, 'cols', 'cols', 20, 1);
reflectUnsigned(HTMLTextAreaElement, 'rows', 'rows', 2, 1);
reflectString(HTMLTextAreaElement, 'dirName', 'dirname');
reflectBoolean(HTMLTextAreaElement, 'disabled');
reflectLong(HTMLTextAreaElement, 'maxLength', 'maxlength', -1, 0);
reflectLong(HTMLTextAreaElement, 'minLength', 'minlength', -1, 0);
reflectString(HTMLTextAreaElement, 'name');
reflectString(HTMLTextAreaElement, 'placeholder');
reflectBoolean(HTMLTextAreaElement, 'readOnly', 'readonly');
reflectBoolean(HTMLTextAreaElement, 'required');
reflectString(HTMLTextAreaElement, 'wrap');
accessor(HTMLTextAreaElement.prototype, 'type', function () {
  return 'textarea';
});
accessor(
  HTMLTextAreaElement.prototype,
  'defaultValue',
  function () {
    return this.textContent;
  },
  function (value) {
    this.textContent = value;
  },
);
accessor(
  HTMLTextAreaElement.prototype,
  'value',
  function () {
    return this.__otlyraControlValue();
  },
  function (value) {
    this.__otlyraSetControlValue(value === null ? '' : String(value));
  },
);
accessor(HTMLTextAreaElement.prototype, 'textLength', function () {
  return this.value.length;
});
method(HTMLTextAreaElement.prototype, 'select', function select() {});
method(HTMLTextAreaElement.prototype, 'setSelectionRange', function setSelectionRange() {});

reflectString(HTMLLabelElement, 'htmlFor', 'for');
// The control a label is for: the element its `for` names, or the first
// labelable element inside it (HTML §4.10.4).
accessor(HTMLLabelElement.prototype, 'control', function () {
  const target = this.getAttribute('for');
  if (target !== null) {
    const found = (this.ownerDocument || document).getElementById(target);
    return found && /^(button|input|meter|output|progress|select|textarea)$/.test(found.localName) ? found : null;
  }
  return this.querySelector('button, input:not([type=hidden]), meter, output, progress, select, textarea');
});

reflectBoolean(HTMLFieldSetElement, 'disabled');
reflectString(HTMLFieldSetElement, 'name');
accessor(HTMLFieldSetElement.prototype, 'type', function () {
  return 'fieldset';
});
accessor(HTMLFieldSetElement.prototype, 'elements', function () {
  return asCollection([...this.querySelectorAll(LISTED)]);
});
reflectString(HTMLOutputElement, 'name');
tokenList(HTMLOutputElement, 'htmlFor', 'for');
for (const name of ['value', 'defaultValue']) {
  accessor(
    HTMLOutputElement.prototype,
    name,
    function () {
      return this.textContent;
    },
    function (value) {
      this.textContent = value;
    },
  );
}

reflectDouble(HTMLProgressElement, 'max', 'max', 1);
accessor(
  HTMLProgressElement.prototype,
  'value',
  function () {
    const parsed = parseDouble(this.getAttribute('value'));
    return parsed === null || parsed < 0 ? 0 : Math.min(parsed, this.max);
  },
  function (value) {
    this.setAttribute('value', String(Number(value)));
  },
);
accessor(HTMLProgressElement.prototype, 'position', function () {
  return this.hasAttribute('value') ? this.value / this.max : -1;
});
for (const [name, fallback] of [['value', 0], ['min', 0], ['max', 1], ['low', 0], ['high', 1], ['optimum', 0.5]]) {
  reflectDouble(HTMLMeterElement, name, name, fallback);
}

// ----------------------------------------------------- the other elements

reflectBoolean(HTMLDetailsElement, 'open');
reflectString(HTMLDetailsElement, 'name');
reflectBoolean(HTMLDialogElement, 'open');
const kReturnValue = Symbol('returnValue');
accessor(
  HTMLDialogElement.prototype,
  'returnValue',
  function () {
    return this[kReturnValue] ?? '';
  },
  function (value) {
    this[kReturnValue] = String(value);
  },
);
method(HTMLDialogElement.prototype, 'show', function show() {
  this.setAttribute('open', '');
});
method(HTMLDialogElement.prototype, 'showModal', function showModal() {
  this.setAttribute('open', '');
});
method(HTMLDialogElement.prototype, 'close', function close(returnValue) {
  if (!this.hasAttribute('open')) return;
  if (returnValue !== undefined) this.returnValue = returnValue;
  this.removeAttribute('open');
  this.dispatchEvent(new Event('close'));
});
method(HTMLDialogElement.prototype, 'requestClose', function requestClose(returnValue) {
  this.close(returnValue);
});

for (const Interface of [HTMLQuoteElement, HTMLModElement]) reflectUrl(Interface, 'cite');
reflectString(HTMLModElement, 'dateTime', 'datetime');
reflectString(HTMLTimeElement, 'dateTime', 'datetime');
reflectString(HTMLDataElement, 'value');
reflectBoolean(HTMLOListElement, 'reversed');
reflectLong(HTMLOListElement, 'start', 'start', 1);
reflectString(HTMLOListElement, 'type');
reflectLong(HTMLLIElement, 'value', 'value', 0);
reflectString(HTMLSlotElement, 'name');
method(HTMLSlotElement.prototype, 'assignedNodes', function assignedNodes() {
  return [];
});
method(HTMLSlotElement.prototype, 'assignedElements', function assignedElements() {
  return [];
});

// Tables, as far as a page that builds one row by row needs them
// (HTML §4.9).
reflectUnsigned(HTMLTableCellElement, 'colSpan', 'colspan', 1, 1, 1000);
reflectUnsigned(HTMLTableCellElement, 'rowSpan', 'rowspan', 1, 0, 65534);
reflectString(HTMLTableCellElement, 'headers');
reflectString(HTMLTableCellElement, 'scope');
reflectString(HTMLTableCellElement, 'abbr');
accessor(HTMLTableCellElement.prototype, 'cellIndex', function () {
  const row = this.parentElement;
  return row && row.localName === 'tr' ? [...row.cells].indexOf(this) : -1;
});
reflectUnsigned(HTMLTableColElement, 'span', 'span', 1, 1, 1000);

const rowsOf = (container) =>
  [...container.children].filter((child) => child.localName === 'tr');
accessor(HTMLTableRowElement.prototype, 'cells', function () {
  return asCollection([...this.children].filter((child) => child.localName === 'td' || child.localName === 'th'));
});
accessor(HTMLTableRowElement.prototype, 'rowIndex', function () {
  const table = this.closest('table');
  return table ? [...table.rows].indexOf(this) : -1;
});
accessor(HTMLTableRowElement.prototype, 'sectionRowIndex', function () {
  const parent = this.parentElement;
  return parent ? rowsOf(parent).indexOf(this) : -1;
});
method(HTMLTableRowElement.prototype, 'insertCell', function insertCell(index = -1) {
  const cell = (this.ownerDocument || document).createElement('td');
  const cells = this.cells;
  if (index === -1 || index === cells.length) this.appendChild(cell);
  else if (index >= 0 && index < cells.length) this.insertBefore(cell, cells[index]);
  else throw new DOMException('no cell at ' + index, 'IndexSizeError');
  return cell;
});
method(HTMLTableRowElement.prototype, 'deleteCell', function deleteCell(index) {
  const cells = this.cells;
  const at = index === -1 ? cells.length - 1 : index;
  if (at < 0 || at >= cells.length) throw new DOMException('no cell at ' + index, 'IndexSizeError');
  cells[at].remove();
});
accessor(HTMLTableSectionElement.prototype, 'rows', function () {
  return asCollection(rowsOf(this));
});
const insertRowInto = (container, index) => {
  const row = (container.ownerDocument || document).createElement('tr');
  const rows = rowsOf(container);
  if (index === -1 || index === rows.length) container.appendChild(row);
  else if (index >= 0 && index < rows.length) container.insertBefore(row, rows[index]);
  else throw new DOMException('no row at ' + index, 'IndexSizeError');
  return row;
};
method(HTMLTableSectionElement.prototype, 'insertRow', function insertRow(index = -1) {
  return insertRowInto(this, index);
});
method(HTMLTableSectionElement.prototype, 'deleteRow', function deleteRow(index) {
  const rows = rowsOf(this);
  const at = index === -1 ? rows.length - 1 : index;
  if (at >= 0 && at < rows.length) rows[at].remove();
});
const childNamed = (table, name) => [...table.children].find((child) => child.localName === name) || null;
accessor(HTMLTableElement.prototype, 'caption', function () {
  return childNamed(this, 'caption');
});
accessor(HTMLTableElement.prototype, 'tHead', function () {
  return childNamed(this, 'thead');
});
accessor(HTMLTableElement.prototype, 'tFoot', function () {
  return childNamed(this, 'tfoot');
});
accessor(HTMLTableElement.prototype, 'tBodies', function () {
  return asCollection([...this.children].filter((child) => child.localName === 'tbody'));
});
accessor(HTMLTableElement.prototype, 'rows', function () {
  const head = this.tHead ? rowsOf(this.tHead) : [];
  const foot = this.tFoot ? rowsOf(this.tFoot) : [];
  const middle = [];
  for (const child of this.children) {
    if (child.localName === 'tr') middle.push(child);
    else if (child.localName === 'tbody') middle.push(...rowsOf(child));
  }
  return asCollection([...head, ...middle, ...foot]);
});
method(HTMLTableElement.prototype, 'insertRow', function insertRow(index = -1) {
  const body = this.tBodies[this.tBodies.length - 1];
  if (body) return insertRowInto(body, index === -1 ? -1 : Math.min(index, rowsOf(body).length));
  const created = (this.ownerDocument || document).createElement('tbody');
  this.appendChild(created);
  return insertRowInto(created, -1);
});
method(HTMLTableElement.prototype, 'deleteRow', function deleteRow(index) {
  const rows = [...this.rows];
  const at = index === -1 ? rows.length - 1 : index;
  if (at >= 0 && at < rows.length) rows[at].remove();
});
for (const [name, tag] of [['createTHead', 'thead'], ['createTFoot', 'tfoot'], ['createCaption', 'caption'], ['createTBody', 'tbody']]) {
  method(HTMLTableElement.prototype, name, function () {
    const existing = tag === 'tbody' ? null : childNamed(this, tag);
    if (existing) return existing;
    const created = (this.ownerDocument || document).createElement(tag);
    if (tag === 'caption' || tag === 'thead') this.insertBefore(created, this.firstChild);
    else this.appendChild(created);
    return created;
  });
}

// The presentational leftovers a page still sets as properties.
for (const [Interface, names] of [
  [HTMLBodyElement, ['text', 'link', 'vLink', 'aLink', 'bgColor', 'background']],
  [HTMLHeadingElement, ['align']],
  [HTMLParagraphElement, ['align']],
  [HTMLDivElement, ['align']],
  [HTMLHRElement, ['align', 'color', 'size', 'width']],
  [HTMLBRElement, ['clear']],
  [HTMLTableElement, ['align', 'border', 'frame', 'rules', 'summary', 'width', 'bgColor', 'cellPadding', 'cellSpacing']],
  [HTMLTableCellElement, ['align', 'axis', 'height', 'width', 'ch', 'chOff', 'vAlign', 'bgColor']],
  [HTMLTableRowElement, ['align', 'ch', 'chOff', 'vAlign', 'bgColor']],
  [HTMLFontElement, ['color', 'face', 'size']],
]) {
  for (const name of names) reflectString(Interface, name, name.toLowerCase());
}
reflectBoolean(HTMLHRElement, 'noShade', 'noshade');

// The legacy factory functions (HTML §4.8.4.1, §4.8.10, §4.10.10).
const factory = (name, Interface, make) => {
  function Factory(...args) {
    if (new.target === undefined) throw new TypeError("Failed to construct '" + name + "': use 'new'");
    return make(...args);
  }
  Object.defineProperty(Factory, 'name', { value: name });
  Factory.prototype = Interface.prototype;
  defineGlobal(name, Factory);
};
factory('Image', HTMLImageElement, (width, height) => {
  const image = document.createElement('img');
  if (width !== undefined) image.width = width;
  if (height !== undefined) image.height = height;
  return image;
});
factory('Audio', HTMLAudioElement, (src) => {
  const audio = document.createElement('audio');
  audio.setAttribute('preload', 'auto');
  if (src !== undefined) audio.src = src;
  return audio;
});
factory('Option', HTMLOptionElement, (text = '', value, defaultSelected = false, selected = false) => {
  const option = document.createElement('option');
  if (text !== '') option.appendChild(document.createTextNode(String(text)));
  if (value !== undefined) option.value = value;
  if (defaultSelected || selected) option.setAttribute('selected', '');
  return option;
});
