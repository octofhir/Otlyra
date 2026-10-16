// Where the page is, and how it asks to be somewhere else.
//
// The address is read back from the host on every access, because a
// navigation replaces it. The setters do not navigate — they say so, and the
// browser does it when this turn is over; a binding that navigated in place
// would be tearing down the document the turn is standing in.
const locationObject = {
  assign(href) {
    nativeNavigate(String(href), false);
  },
  replace(href) {
    nativeNavigate(String(href), true);
  },
  reload() {
    nativeReload();
  },
  toString() {
    return nativeUrl();
  },
};

const part = (name, fallback) => {
  Object.defineProperty(locationObject, name, {
    get() {
      try {
        return new URL(nativeUrl())[name];
      } catch (_) {
        return fallback;
      }
    },
    set(next) {
      // `location.pathname = '/x'` is a navigation to the address that
      // results, which is what a URL with that part replaced spells out.
      try {
        const url = new URL(nativeUrl());
        url[name] = next;
        nativeNavigate(url.href, false);
      } catch (_) {
        nativeNavigate(String(next), false);
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
      return new URL(nativeUrl()).origin;
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
    nativeNavigate(String(next), false);
  },
  enumerable: false,
  configurable: true,
});

Object.defineProperty(Document.prototype, 'location', {
  get() {
    return locationObject;
  },
  set(next) {
    nativeNavigate(String(next), false);
  },
  enumerable: false,
  configurable: true,
});

// The session history (HTML §7.4), as far as one document reaches: the
// entries it made itself with `pushState`, each with its address and its
// state. The document is not fetched again for any of them; `location` and
// the browser's address bar follow its URL, and moving between them fires
// `popstate`. Going back past the first of them is the browser's back
// button, which script does not reach yet.
const nativeSetUrl = take(Document, '__setUrl');
const HISTORY_LIMIT = 100;
let historyEntries = null;
let historyIndex = 0;
let scrollRestoration = 'auto';

const sessionEntries = () => {
  const here = nativeUrl();
  // The first look, or a document this isolate is now lent for: its own
  // address is the one entry it starts with.
  if (historyEntries === null || historyEntries[historyIndex].url !== here) {
    historyEntries = [{ url: here, state: null }];
    historyIndex = 0;
  }
  return historyEntries;
};

const serializeState = (state) => {
  if (state === undefined || state === null) return null;
  return typeof structuredClone === 'function' ? structuredClone(state) : state;
};

// HTML §7.4.4 "shared history push/replace state steps", steps 1–5: the URL
// is parsed against the document and may not leave its origin.
const historyTarget = (url) => {
  const here = new URL(nativeUrl());
  if (url === undefined || url === null) return here.href;
  let target;
  try {
    target = new URL(String(url), documentBase());
  } catch (_) {
    throw new DOMException("'" + url + "' is not a URL", 'SecurityError');
  }
  if (target.origin !== here.origin) {
    throw new DOMException(
      "a history entry for '" + target.href + "' cannot be made in a document at '" + here.origin + "'",
      'SecurityError',
    );
  }
  return target.href;
};

class History {
  constructor() {
    throw new TypeError('Illegal constructor');
  }

  get length() {
    return sessionEntries().length;
  }

  get state() {
    return sessionEntries()[historyIndex].state;
  }

  get scrollRestoration() {
    return scrollRestoration;
  }

  set scrollRestoration(value) {
    if (value === 'auto' || value === 'manual') scrollRestoration = value;
  }

  pushState(state, unused, url) {
    const entries = sessionEntries();
    const href = historyTarget(url);
    entries.splice(historyIndex + 1, entries.length, { url: href, state: serializeState(state) });
    historyIndex += 1;
    if (entries.length > HISTORY_LIMIT) {
      entries.shift();
      historyIndex -= 1;
    }
    nativeSetUrl(href);
  }

  replaceState(state, unused, url) {
    const entries = sessionEntries();
    const href = historyTarget(url);
    entries[historyIndex] = { url: href, state: serializeState(state) };
    nativeSetUrl(href);
  }

  go(delta) {
    const step = Math.trunc(Number(delta)) || 0;
    if (step === 0) {
      location.reload();
      return;
    }
    const entries = sessionEntries();
    const next = historyIndex + step;
    if (next < 0 || next >= entries.length) return;
    // A traversal is a task of its own: `popstate` arrives after the
    // script that asked has finished.
    setTimeout(() => {
      const from = entries[historyIndex];
      historyIndex = next;
      const to = entries[historyIndex];
      nativeSetUrl(to.url);
      globalThis.dispatchEvent(new PopStateEvent('popstate', { state: to.state }));
      const strip = (href) => href.split('#')[0];
      if (strip(from.url) === strip(to.url) && from.url !== to.url) {
        globalThis.dispatchEvent(
          new HashChangeEvent('hashchange', { oldURL: from.url, newURL: to.url }),
        );
      }
    }, 0);
  }

  back() {
    this.go(-1);
  }

  forward() {
    this.go(1);
  }
}
Object.defineProperty(History.prototype, Symbol.toStringTag, {
  value: 'History',
  configurable: true,
});
defineGlobal('History', History);
defineGlobal('history', Object.create(History.prototype));
