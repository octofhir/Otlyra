// ---------------------------------------------------------------- A worker's global
// What makes an isolate a dedicated worker's (HTML §10.2.1.4): `self`, an
// event target that hears the page's messages, `postMessage`, `close`,
// `importScripts`, `location` and `navigator` — and `fetch`, which goes
// through the page and the browser's network rather than the engine's own.
// Runs after `clone.js`, in the same function scope.

const nativePost = __otlyraWorkerPost;
const nativeNext = __otlyraWorkerNext;
const nativeFetch = __otlyraWorkerFetch;
const nativeClose = __otlyraWorkerClose;
for (const name of ['__otlyraWorkerPost', '__otlyraWorkerNext', '__otlyraWorkerFetch', '__otlyraWorkerClose']) {
  delete globalThis[name];
}

const hidden = (name, value) =>
  Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });

// A body fetched through the page: status, final address, and bytes.
const fetchThroughPage = (url, script) => {
  const answer = nativeFetch(url, script);
  const first = answer.indexOf('\n');
  const second = answer.indexOf('\n', first + 1);
  return {
    status: Number(answer.slice(0, first)),
    url: answer.slice(first + 1, second),
    bytes: textToBytes(answer.slice(second + 1)),
  };
};

class WorkerGlobalScope extends EventTarget {}
class DedicatedWorkerGlobalScope extends WorkerGlobalScope {}
class WorkerLocation {
  #url;
  constructor(href) {
    this.#url = new URL(href);
  }
  get href() { return this.#url.href; }
  get origin() { return this.#url.origin; }
  get protocol() { return this.#url.protocol; }
  get host() { return this.#url.host; }
  get hostname() { return this.#url.hostname; }
  get port() { return this.#url.port; }
  get pathname() { return this.#url.pathname; }
  get search() { return this.#url.search; }
  get hash() { return this.#url.hash; }
  toString() { return this.#url.href; }
}

// The global is the event target: its listeners are kept on one made for it,
// and the methods are the global's own.
const events = new EventTarget();
for (const name of ['addEventListener', 'removeEventListener', 'dispatchEvent']) {
  hidden(name, (...args) => events[name](...args));
}
let onmessage = null;
let onerror = null;
let onmessageerror = null;
Object.defineProperties(globalThis, {
  onmessage: { get: () => onmessage, set: (value) => { onmessage = typeof value === 'function' ? value : null; }, configurable: true },
  onerror: { get: () => onerror, set: (value) => { onerror = typeof value === 'function' ? value : null; }, configurable: true },
  onmessageerror: { get: () => onmessageerror, set: (value) => { onmessageerror = typeof value === 'function' ? value : null; }, configurable: true },
});

hidden('WorkerGlobalScope', WorkerGlobalScope);
hidden('DedicatedWorkerGlobalScope', DedicatedWorkerGlobalScope);
hidden('WorkerLocation', WorkerLocation);
hidden('self', globalThis);
hidden('postMessage', function postMessage(message) {
  nativePost(cloneSerialize(message));
});
hidden('close', function close() {
  nativeClose();
});
hidden('importScripts', function importScripts(...urls) {
  for (const url of urls) {
    const address = new URL(String(url), location.href).href;
    const { status, url: from, bytes } = fetchThroughPage(address, true);
    if (status < 200 || status >= 300) {
      throw new DOMException(`The script at '${address}' failed to load.`, 'NetworkError');
    }
    (0, eval)(`${new TextDecoder().decode(bytes)}\n//# sourceURL=${from}`);
  }
});
hidden('fetch', function fetch(input, init = {}) {
  try {
    const address = new URL(String(input instanceof Request ? input.url : input), location.href).href;
    if ((init.method ?? 'GET').toUpperCase() !== 'GET') {
      return Promise.reject(new TypeError('Only GET requests leave a worker yet.'));
    }
    const { status, url, bytes } = fetchThroughPage(address, false);
    const response = new Response(status === 204 || status === 304 ? null : bytes, { status });
    Object.defineProperty(response, 'url', { value: url });
    return Promise.resolve(response);
  } catch (error) {
    return Promise.reject(new TypeError(`Failed to fetch: ${error.message}`));
  }
});

hidden('__otlyraWorkerSetup', (href) => {
  hidden('location', new WorkerLocation(href));
  const userAgent = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36 Otlyra/0.1';
  hidden('navigator', {
    userAgent,
    appVersion: userAgent.slice('Mozilla/'.length),
    language: 'en-US',
    languages: ['en-US', 'en'],
    onLine: true,
    hardwareConcurrency: 4,
    platform: 'MacIntel',
  });
});

// Hear what the page posted, one message to each event.
hidden('__otlyraWorkerDispatch', () => {
  let data;
  while ((data = nativeNext()) !== null) {
    let value;
    try {
      value = cloneDeserialize(data);
    } catch (_) {
      const event = new MessageEvent('messageerror', { data: null });
      if (onmessageerror) onmessageerror.call(globalThis, event);
      events.dispatchEvent(event);
      continue;
    }
    const event = new MessageEvent('message', { data: value });
    if (onmessage) {
      try {
        onmessage.call(globalThis, event);
      } catch (error) {
        if (!onerror || onerror.call(globalThis, String(error?.message ?? error), location.href, 0, 0, error) !== true) throw error;
      }
    }
    events.dispatchEvent(event);
  }
});
