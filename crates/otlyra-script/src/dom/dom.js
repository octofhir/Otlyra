// The half of the DOM that is better written in JavaScript than generated.
//
// Everything here is glue over the native classes: the `window` self-references
// a page expects, the singleton `document`, `classList` (which is a live view
// over one attribute and nothing more), and listener registration that keeps
// its callbacks in JavaScript where the collector can see them.
//
// Event *propagation* is deliberately not here. Capture and bubble need the
// ancestor tree and are browser semantics rather than glue, so they arrive with
// the real EventTarget; what this gives a page is registration that works and a
// dispatch that reaches the target itself.

(function () {
  'use strict';

  // `window` is the global object, and everything a page uses to say "the
  // window I am in" is the same object. A page in no frame is its own top.
  const defineGlobal = (name, value) => {
    Object.defineProperty(globalThis, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  defineGlobal('window', globalThis);
  defineGlobal('self', globalThis);
  defineGlobal('top', globalThis);
  defineGlobal('parent', globalThis);

  // `document` is fetched on first use rather than now: this whole file is
  // evaluated while the isolate is being built, and at that moment there is no
  // page yet. Once fetched it is kept, because a page compares documents for
  // identity and two wrappers for the same tree would not be equal.
  let documentSingleton = null;
  Object.defineProperty(globalThis, 'document', {
    get() {
      if (documentSingleton === null) documentSingleton = Document.__self();
      return documentSingleton;
    },
    set(value) {
      documentSingleton = value;
    },
    enumerable: false,
    configurable: true,
  });

  // `class` as a list. A DOMTokenList is a live view of the attribute: reading
  // it re-reads, and every mutation writes the attribute back, because the
  // attribute is where style will look.
  class DOMTokenList {
    #owner;

    constructor(owner) {
      this.#owner = owner;
    }

    #tokens() {
      const value = this.#owner.getAttribute('class');
      return value ? value.split(/\s+/).filter(Boolean) : [];
    }

    #write(tokens) {
      this.#owner.setAttribute('class', tokens.join(' '));
    }

    get length() {
      return this.#tokens().length;
    }

    get value() {
      return this.#owner.getAttribute('class') || '';
    }

    set value(next) {
      this.#owner.setAttribute('class', String(next));
    }

    item(index) {
      const tokens = this.#tokens();
      return index >= 0 && index < tokens.length ? tokens[index] : null;
    }

    contains(token) {
      return this.#tokens().indexOf(String(token)) !== -1;
    }

    add(...tokens) {
      const current = this.#tokens();
      for (const token of tokens) {
        const name = String(token);
        if (current.indexOf(name) === -1) current.push(name);
      }
      this.#write(current);
    }

    remove(...tokens) {
      const drop = tokens.map(String);
      this.#write(this.#tokens().filter((token) => drop.indexOf(token) === -1));
    }

    toggle(token, force) {
      const name = String(token);
      const has = this.contains(name);
      const wanted = force === undefined ? !has : Boolean(force);
      if (wanted === has) return has;
      if (wanted) this.add(name);
      else this.remove(name);
      return wanted;
    }

    replace(from, to) {
      const tokens = this.#tokens();
      const at = tokens.indexOf(String(from));
      if (at === -1) return false;
      tokens[at] = String(to);
      this.#write(tokens);
      return true;
    }

    toString() {
      return this.value;
    }

    forEach(callback, thisArg) {
      this.#tokens().forEach(callback, thisArg);
    }

    [Symbol.iterator]() {
      return this.#tokens()[Symbol.iterator]();
    }
  }

  // A fresh list per access, and correct because a DOMTokenList holds no state
  // of its own: it reads the attribute when asked and writes it when changed.
  // Caching one per element would need wrapper identity, which we do not have
  // yet — see the note at the top of `node.rs`.
  Object.defineProperty(Element.prototype, 'classList', {
    get() {
      return new DOMTokenList(this);
    },
    enumerable: false,
    configurable: true,
  });

  // Listener registration. The callbacks live in a JavaScript map keyed by the
  // target object, so the collector traces them; nothing about a listener is
  // held on the Rust side, where it would be invisible to it.
  // A strong Map rather than a WeakMap, for two reasons. The engine will not
  // take a host object as a WeakMap key, and wrappers have no identity yet, so
  // a weak table keyed on them would drop every listener the moment the wrapper
  // that registered it went away. Both are fixed by the same work — wrapper
  // identity — and until then the table lives as long as the page does.
  const listeners = new Map();

  const listenersFor = (target, type, create) => {
    let byType = listeners.get(target);
    if (!byType) {
      if (!create) return undefined;
      byType = new Map();
      listeners.set(target, byType);
    }
    let list = byType.get(type);
    if (!list) {
      if (!create) return undefined;
      list = [];
      byType.set(type, list);
    }
    return list;
  };

  const eventTarget = {
    addEventListener(type, callback, options) {
      if (!callback) return;
      const list = listenersFor(this, String(type), true);
      const once = Boolean(options && typeof options === 'object' && options.once);
      if (!list.some((entry) => entry.callback === callback)) {
        list.push({ callback, once });
      }
    },

    removeEventListener(type, callback) {
      const list = listenersFor(this, String(type), false);
      if (!list) return;
      const at = list.findIndex((entry) => entry.callback === callback);
      if (at !== -1) list.splice(at, 1);
    },

    dispatchEvent(event) {
      if (!event || typeof event.type !== 'string') {
        throw new TypeError('dispatchEvent expects an Event');
      }
      const list = listenersFor(this, event.type, false);
      if (!list) return true;
      try {
        Object.defineProperty(event, 'target', { value: this, configurable: true });
        Object.defineProperty(event, 'currentTarget', { value: this, configurable: true });
      } catch (_) {
        // A frozen event object is still dispatchable; the listeners simply see
        // whatever it says about itself.
      }
      for (const entry of list.slice()) {
        if (entry.once) this.removeEventListener(event.type, entry.callback);
        const handler =
          typeof entry.callback === 'function' ? entry.callback : entry.callback.handleEvent;
        try {
          handler.call(this, event);
        } catch (error) {
          console.error(error);
        }
      }
      return !event.defaultPrevented;
    },
  };

  for (const target of [globalThis, Node.prototype]) {
    for (const name of ['addEventListener', 'removeEventListener', 'dispatchEvent']) {
      Object.defineProperty(target, name, {
        value: eventTarget[name],
        writable: true,
        enumerable: false,
        configurable: true,
      });
    }
  }

  // A media query the page can ask about. Answered from what we know, which is
  // one window with no preference declared; the listener half exists so that a
  // page that subscribes does not throw on a browser that never changes its
  // answer.
  defineGlobal('matchMedia', function matchMedia(query) {
    return {
      media: String(query),
      matches: false,
      onchange: null,
      addListener() {},
      removeListener() {},
      addEventListener() {},
      removeEventListener() {},
      dispatchEvent() {
        return false;
      },
    };
  });

  // Animation frames and timers, parked until the page has somewhere to run
  // them. There is no timer wheel and no frame loop yet — both are the next
  // milestone — so what this gives a page is registration that works and a
  // single flush the host performs when the document is finished. That is not
  // the event loop; it is the difference between a page whose deferred work
  // never happens and one whose deferred work throws immediately.
  const frameCallbacks = new Map();
  let nextFrameHandle = 1;

  defineGlobal('requestAnimationFrame', function requestAnimationFrame(callback) {
    if (typeof callback !== 'function') {
      throw new TypeError('requestAnimationFrame expects a function');
    }
    const handle = nextFrameHandle++;
    frameCallbacks.set(handle, callback);
    return handle;
  });

  defineGlobal('cancelAnimationFrame', function cancelAnimationFrame(handle) {
    frameCallbacks.delete(handle);
  });

  const timers = new Map();
  let nextTimerHandle = 1;

  const schedule = (callback, delay, args, repeating) => {
    if (typeof callback !== 'function') return 0;
    const handle = nextTimerHandle++;
    timers.set(handle, { callback, args, delay: Number(delay) || 0, repeating });
    return handle;
  };

  defineGlobal('setTimeout', function setTimeout(callback, delay, ...args) {
    return schedule(callback, delay, args, false);
  });
  defineGlobal('setInterval', function setInterval(callback, delay, ...args) {
    return schedule(callback, delay, args, true);
  });
  defineGlobal('clearTimeout', function clearTimeout(handle) {
    timers.delete(handle);
  });
  defineGlobal('clearInterval', function clearInterval(handle) {
    timers.delete(handle);
  });
  // `setImmediate` is Node's, and the engine ships one that wants a scheduler
  // the host has not installed — so a page that reaches for it gets a throw
  // rather than a missing global to feature-detect. It belongs on this queue
  // with the rest: a zero-delay one-shot is what it is.
  defineGlobal('setImmediate', function setImmediate(callback, ...args) {
    return schedule(callback, 0, args, false);
  });
  defineGlobal('clearImmediate', function clearImmediate(handle) {
    timers.delete(handle);
  });

  // The host's one call into all of the above. Deliberately bounded: a page
  // whose timer schedules another timer would otherwise keep this running for
  // as long as it liked, and this is not the place to give it that.
  const FLUSH_LIMIT = 256;

  let loadEventsFired = false;

  defineGlobal('__otlyraFlushDeferred', function __otlyraFlushDeferred(fireLoadEvents) {
    let ran = 0;

    // The two events every page waits for. They come first because everything
    // a page defers is registered from one of them — and they happen once, when
    // the host says every script that was going to arrive has arrived.
    if (fireLoadEvents && !loadEventsFired) {
      loadEventsFired = true;
      for (const [target, type] of [
        [document, 'DOMContentLoaded'],
        [globalThis, 'load'],
      ]) {
        try {
          target.dispatchEvent(new Event(type));
        } catch (error) {
          console.error(error);
        }
        ran++;
      }
    }

    const frames = [...frameCallbacks.entries()];
    frameCallbacks.clear();
    for (const [, callback] of frames) {
      try {
        callback(0);
      } catch (error) {
        console.error(error);
      }
      ran++;
    }

    // Sorted by the delay each asked for, which is the only ordering we can
    // honour without a clock: two timers with the same delay run in the order
    // they were registered, because a Map iterates in insertion order.
    const due = [...timers.entries()].sort((a, b) => a[1].delay - b[1].delay);
    for (const [handle, timer] of due) {
      if (ran >= FLUSH_LIMIT) break;
      if (!timers.has(handle)) continue;
      // Intervals run once here. A repeating timer in a flush with no clock is
      // a loop, and a page is not owed one.
      timers.delete(handle);
      try {
        timer.callback(...timer.args);
      } catch (error) {
        console.error(error);
      }
      ran++;
    }
    return ran;
  });

  // The attributes a page sets as properties. `element.value = 'x'` is not a
  // plain assignment on the platform: it reflects into the content attribute,
  // which is where the form serializer and the cascade look for it. Without
  // this a script builds a form whose fields are all empty.
  //
  // `value` is the approximation in the list: on a real input it is a property
  // with a dirty flag of its own, and only its default comes from the
  // attribute. For a form built by script — which is what this is for — the two
  // are the same thing.
  const REFLECTED = [
    'name',
    'type',
    'value',
    'action',
    'method',
    'enctype',
    'target',
    'href',
    'src',
    'alt',
    'rel',
    'title',
    'placeholder',
  ];

  for (const attribute of REFLECTED) {
    Object.defineProperty(Element.prototype, attribute, {
      get() {
        return this.getAttribute(attribute) ?? '';
      },
      set(next) {
        this.setAttribute(attribute, String(next));
      },
      enumerable: false,
      configurable: true,
    });
  }

  // `element.style`, which is a live view of one attribute the way `classList`
  // is a live view of another. A page reads it to feel out what the browser
  // supports — React asks `"animation" in element.style` before it decides what
  // to listen for — and writes it to move things about.
  //
  // The property list is fixed rather than open-ended, because `in` has to
  // answer honestly: a style object that claimed every name would tell a page
  // we support things we do not.
  const STYLE_PROPERTIES = [
    'animation', 'animationDelay', 'animationDirection', 'animationDuration',
    'animationFillMode', 'animationIterationCount', 'animationName',
    'animationTimingFunction', 'background', 'backgroundColor', 'backgroundImage',
    'backgroundPosition', 'backgroundRepeat', 'backgroundSize', 'border',
    'borderBottom', 'borderColor', 'borderLeft', 'borderRadius', 'borderRight',
    'borderStyle', 'borderTop', 'borderWidth', 'bottom', 'boxShadow', 'boxSizing',
    'clip', 'color', 'columnGap', 'content', 'cursor', 'direction', 'display',
    'flex', 'flexBasis', 'flexDirection', 'flexGrow', 'flexShrink', 'flexWrap',
    'float', 'font', 'fontFamily', 'fontSize', 'fontStyle', 'fontWeight', 'gap',
    'gridArea', 'gridColumn', 'gridRow', 'gridTemplateColumns', 'gridTemplateRows',
    'height', 'justifyContent', 'alignItems', 'alignSelf', 'left', 'letterSpacing',
    'lineHeight', 'listStyle', 'margin', 'marginBottom', 'marginLeft',
    'marginRight', 'marginTop', 'maxHeight', 'maxWidth', 'minHeight', 'minWidth',
    'objectFit', 'opacity', 'order', 'outline', 'overflow', 'overflowX',
    'overflowY', 'padding', 'paddingBottom', 'paddingLeft', 'paddingRight',
    'paddingTop', 'pointerEvents', 'position', 'right', 'rowGap', 'textAlign',
    'textDecoration', 'textOverflow', 'textTransform', 'top', 'transform',
    'transformOrigin', 'transition', 'transitionDelay', 'transitionDuration',
    'transitionProperty', 'transitionTimingFunction', 'userSelect',
    'verticalAlign', 'visibility', 'whiteSpace', 'width', 'wordBreak', 'zIndex',
  ];

  const hyphenate = (name) => name.replace(/[A-Z]/g, (letter) => '-' + letter.toLowerCase());
  const camelCase = (name) =>
    name.replace(/-([a-z])/g, (_all, letter) => letter.toUpperCase());

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
        const name = part.slice(0, at).trim().toLowerCase();
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
      return this.#declarations().get(String(name).trim().toLowerCase()) ?? '';
    }

    setProperty(name, value) {
      const map = this.#declarations();
      const property = String(name).trim().toLowerCase();
      if (value === null || value === undefined || value === '') map.delete(property);
      else map.set(property, String(value));
      this.#write(map);
    }

    removeProperty(name) {
      const map = this.#declarations();
      const property = String(name).trim().toLowerCase();
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
    void camelCase;
  }

  Object.defineProperty(Element.prototype, 'style', {
    get() {
      return new CSSStyleDeclaration(this);
    },
    enumerable: false,
    configurable: true,
  });

  // Where the page is, and how it asks to be somewhere else.
  //
  // The address is read back from the host on every access, because a
  // navigation replaces it. The setters do not navigate — they say so, and the
  // browser does it when this turn is over; a binding that navigated in place
  // would be tearing down the document the turn is standing in.
  const locationObject = {
    assign(href) {
      Document.__navigate(String(href), false);
    },
    replace(href) {
      Document.__navigate(String(href), true);
    },
    reload() {
      Document.__reload();
    },
    toString() {
      return Document.__url();
    },
  };

  const part = (name, fallback) => {
    Object.defineProperty(locationObject, name, {
      get() {
        try {
          return new URL(Document.__url())[name];
        } catch (_) {
          return fallback;
        }
      },
      set(next) {
        // `location.pathname = '/x'` is a navigation to the address that
        // results, which is what a URL with that part replaced spells out.
        try {
          const url = new URL(Document.__url());
          url[name] = next;
          Document.__navigate(url.href, false);
        } catch (_) {
          Document.__navigate(String(next), false);
        }
      },
      enumerable: true,
      configurable: true,
    });
  };

  for (const name of [
    'href',
    'protocol',
    'host',
    'hostname',
    'port',
    'pathname',
    'search',
    'hash',
  ]) {
    part(name, '');
  }

  Object.defineProperty(locationObject, 'origin', {
    get() {
      try {
        return new URL(Document.__url()).origin;
      } catch (_) {
        return 'null';
      }
    },
    enumerable: true,
    configurable: true,
  });

  Object.defineProperty(globalThis, 'location', {
    get() {
      return locationObject;
    },
    // `window.location = '…'` is the oldest way to navigate there is.
    set(next) {
      Document.__navigate(String(next), false);
    },
    enumerable: false,
    configurable: true,
  });

  Object.defineProperty(Document.prototype, 'location', {
    get() {
      return locationObject;
    },
    set(next) {
      Document.__navigate(String(next), false);
    },
    enumerable: false,
    configurable: true,
  });

  // The rest of what a page reaches for on its way to being drawn. Each of
  // these is either a fact about this browser (`sendBeacon` cannot send, so it
  // says so) or a shape that lets a page carry on to the part that renders.
  const define = (object, name, value) => {
    if (!object || name in object) return;
    Object.defineProperty(object, name, {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
  };

  Object.defineProperty(Node.prototype, 'parentNode', {
    get() {
      const parent = this.parentElement;
      if (parent) return parent;
      return this === document || !this.isConnected ? null : document;
    },
    enumerable: false,
    configurable: true,
  });

  // `innerText` is what a page reads when it wants text a reader could have
  // seen: collapsed whitespace, no `<script>` and no `display: none`. That is a
  // question about layout, and there is no layout here yet — so this is
  // `textContent`, which is the same answer for every element that is not
  // rendered and a whitespace-noisy one for those that are.
  //
  // The case it is here for is exactly the not-rendered one: a page keeps its
  // server-rendered state in a `<script type="application/json">` and reads it
  // back with `innerText`, and the spec says an element with no layout box
  // answers with its `textContent`.
  Object.defineProperty(Element.prototype, 'innerText', {
    get() {
      return this.textContent;
    },
    set(next) {
      this.textContent = next;
    },
    enumerable: false,
    configurable: true,
  });

  const byTagName = function getElementsByTagName(name) {
    return String(name) === '*' ? this.querySelectorAll('*') : this.querySelectorAll(String(name));
  };
  const byClassName = function getElementsByClassName(names) {
    const selector = String(names)
      .split(/\s+/)
      .filter(Boolean)
      .map((token) => `.${token}`)
      .join('');
    return selector ? this.querySelectorAll(selector) : [];
  };

  for (const proto of [Document.prototype, Element.prototype]) {
    define(proto, 'getElementsByTagName', byTagName);
    define(proto, 'getElementsByClassName', byClassName);
  }

  // Geometry, which needs layout and has none to read yet. Zeroes rather than
  // an exception: a page that measures an element and gets zero lays itself out
  // as if the element were empty, which is wrong but renderable, where a throw
  // takes the rest of the script with it.
  define(Element.prototype, 'getBoundingClientRect', function getBoundingClientRect() {
    return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0 };
  });

  if (typeof CustomEvent === 'undefined' && typeof Event !== 'undefined') {
    class CustomEventShim extends Event {
      constructor(type, options) {
        super(type, options);
        this.detail = options && 'detail' in options ? options.detail : null;
      }
    }
    defineGlobal('CustomEvent', CustomEventShim);
  }

  if (typeof navigator === 'object' && navigator) {
    // It cannot send, and says so rather than pretending: a page that believes
    // its beacon left is a page that will not try the request another way.
    define(navigator, 'sendBeacon', () => false);
    define(navigator, 'language', 'en-US');
    define(navigator, 'languages', ['en-US']);
    define(navigator, 'onLine', true);
    define(navigator, 'cookieEnabled', true);
  }

  if (typeof performance === 'object' && performance) {
    define(performance, 'getEntriesByType', () => []);
    define(performance, 'getEntriesByName', () => []);
    define(performance, 'getEntries', () => []);
    define(performance, 'mark', () => undefined);
    define(performance, 'measure', () => undefined);
    define(performance, 'timing', {});
    define(performance, 'navigation', { type: 0, redirectCount: 0 });
  }

  // An observer that never observes. The alternative is that a page which
  // lazy-loads through one shows nothing at all.
  class NoopObserver {
    observe() {}
    unobserve() {}
    disconnect() {}
    takeRecords() {
      return [];
    }
  }
  if (typeof IntersectionObserver === 'undefined') defineGlobal('IntersectionObserver', NoopObserver);
  if (typeof MutationObserver === 'undefined') defineGlobal('MutationObserver', NoopObserver);
  if (typeof ResizeObserver === 'undefined') defineGlobal('ResizeObserver', NoopObserver);
})();
