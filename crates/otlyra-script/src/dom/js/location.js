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
