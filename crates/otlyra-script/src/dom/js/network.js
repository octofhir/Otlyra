// The page's own requests: `fetch` and `XMLHttpRequest`. The network is the
// browser's — its fetcher, its cookies, its cache and its policy — so the
// isolate never opens a socket: a request is handed to the browser, which
// answers in a later task, and this settles whatever was waiting on it.
//
// What is not here: streaming bodies (a response arrives whole), binary
// request bodies other than text, synchronous `XMLHttpRequest`, upload
// progress, and `Request` bodies. The browser refuses a cross-origin answer
// the page may not read, which reaches script as a network error.

const nativeFetch = take(Document, '__fetch');
const nativeFetchTake = take(Document, '__fetchTake');

// What waits on each request the page made, by its number.
const awaitingResponse = new Map();

defineGlobal('__otlyraFetchSettle', function __otlyraFetchSettle(id) {
  const parts = nativeFetchTake(id);
  const waiting = awaitingResponse.get(id);
  if (!waiting || !parts) return;
  awaitingResponse.delete(id);
  if (parts[0] !== 'ok') {
    waiting.fail(parts[1] || 'network error');
    return;
  }
  const count = Number(parts[3]);
  const headers = [];
  for (let index = 0; index < count; index++) {
    headers.push([parts[4 + index * 2], parts[5 + index * 2]]);
  }
  waiting.succeed({
    status: Number(parts[1]),
    url: parts[2],
    headers,
    body: parts[4 + count * 2],
  });
});

const STATUS_TEXT = {
  200: 'OK',
  201: 'Created',
  202: 'Accepted',
  204: 'No Content',
  206: 'Partial Content',
  301: 'Moved Permanently',
  302: 'Found',
  303: 'See Other',
  304: 'Not Modified',
  307: 'Temporary Redirect',
  308: 'Permanent Redirect',
  400: 'Bad Request',
  401: 'Unauthorized',
  403: 'Forbidden',
  404: 'Not Found',
  405: 'Method Not Allowed',
  409: 'Conflict',
  410: 'Gone',
  422: 'Unprocessable Content',
  429: 'Too Many Requests',
  500: 'Internal Server Error',
  502: 'Bad Gateway',
  503: 'Service Unavailable',
  504: 'Gateway Timeout',
};

const FORM_TYPE = 'application/x-www-form-urlencoded;charset=UTF-8';

// A header list as `name: value` lines, which the native takes.
const headerLines = (pairs) =>
  pairs.map(([name, value]) => String(name).trim() + ': ' + String(value).replace(/[\r\n]/g, ' ')).join('\n');

// The pairs a `HeadersInit` names, in order.
const headerPairs = (init) => {
  if (init === undefined || init === null) return [];
  if (typeof Headers === 'function' && init instanceof Headers) return [...init.entries()];
  if (typeof init[Symbol.iterator] === 'function') return Array.from(init, (pair) => [pair[0], pair[1]]);
  return Object.keys(init).map((name) => [name, init[name]]);
};
const hasHeader = (pairs, name) => pairs.some(([key]) => String(key).toLowerCase() === name);

// A request body as the text the browser sends, and the type it implies.
const requestBody = (body, pairs) => {
  if (body === undefined || body === null) return null;
  if (typeof body === 'string') {
    if (!hasHeader(pairs, 'content-type')) pairs.push(['content-type', 'text/plain;charset=UTF-8']);
    return body;
  }
  if (typeof URLSearchParams === 'function' && body instanceof URLSearchParams) {
    if (!hasHeader(pairs, 'content-type')) pairs.push(['content-type', FORM_TYPE]);
    return body.toString();
  }
  if (typeof FormData === 'function' && body instanceof FormData) {
    // Sent URL-encoded: a multipart body needs a boundary the native would
    // have to write, and files are not sent from here yet.
    const form = new URLSearchParams();
    for (const [name, value] of body) form.append(name, typeof value === 'string' ? value : '');
    if (!hasHeader(pairs, 'content-type')) pairs.push(['content-type', FORM_TYPE]);
    return form.toString();
  }
  if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) {
    return new TextDecoder().decode(body);
  }
  return String(body);
};

const resolveAddress = (url) => new URL(String(url), documentBase()).href;

// Object URLs (File API §8): `blob:` addresses for the Blobs this document
// made, which `fetch` answers from here. The browser never sees them: an
// object URL means something only to the document that minted it.
const objectUrls = new Map();
const mintObjectUrl = () => {
  const origin = location.origin;
  return 'blob:' + (origin && origin !== 'null' ? origin : 'null') + '/' + crypto.randomUUID();
};
Object.defineProperty(URL, 'createObjectURL', {
  value: function createObjectURL(object) {
    if (!(object instanceof Blob)) {
      throw new TypeError("Failed to execute 'createObjectURL' on 'URL': parameter 1 is not of type 'Blob'.");
    }
    const url = mintObjectUrl();
    objectUrls.set(url, object);
    return url;
  },
  writable: true,
  configurable: true,
});
Object.defineProperty(URL, 'revokeObjectURL', {
  value: function revokeObjectURL(url) {
    objectUrls.delete(String(url));
  },
  writable: true,
  configurable: true,
});

// HTML §4 "fetch()" through the browser.
defineGlobal('fetch', function fetch(input, init = {}) {
  const address = typeof input === 'string' || input instanceof URL ? String(input) : null;
  if (address !== null && address.startsWith('blob:')) {
    const blob = objectUrls.get(address.split('#')[0]);
    if (!blob) return Promise.reject(new TypeError('Failed to fetch'));
    const response = new Response(blob, {
      status: 200,
      headers: blob.type ? { 'content-type': blob.type } : {},
    });
    try {
      Object.defineProperty(response, 'url', { value: address.split('#')[0], configurable: true });
    } catch (_) {}
    return Promise.resolve(response);
  }
  return new Promise((resolve, reject) => {
    let url;
    let method = 'GET';
    let pairs = [];
    let signal = init.signal;
    try {
      if (typeof Request === 'function' && input instanceof Request) {
        url = input.url;
        method = input.method;
        pairs = headerPairs(input.headers);
        signal = signal || input.signal;
      } else {
        url = resolveAddress(input);
      }
      if (init.method !== undefined) method = String(init.method).toUpperCase();
      if (init.headers !== undefined) pairs = headerPairs(init.headers);
    } catch (error) {
      reject(new TypeError('Failed to fetch: ' + (error && error.message ? error.message : error)));
      return;
    }
    if (signal && signal.aborted) {
      reject(signal.reason || new DOMException('The operation was aborted.', 'AbortError'));
      return;
    }
    const body = method === 'GET' || method === 'HEAD' ? null : requestBody(init.body, pairs);
    const id = nativeFetch(method, url, headerLines(pairs), body);
    awaitingResponse.set(id, {
      succeed(answer) {
        const nullBody = [101, 204, 205, 304].includes(answer.status) || method === 'HEAD';
        let response;
        try {
          response = new Response(nullBody ? null : answer.body, {
            status: answer.status,
            statusText: STATUS_TEXT[answer.status] || '',
            headers: answer.headers,
          });
        } catch (error) {
          reject(new TypeError('Failed to fetch: ' + error.message));
          return;
        }
        for (const [name, value] of [
          ['url', answer.url],
          ['redirected', answer.url !== url],
        ]) {
          try {
            Object.defineProperty(response, name, { value, configurable: true });
          } catch (_) {}
        }
        resolve(response);
      },
      fail() {
        reject(new TypeError('Failed to fetch'));
      },
    });
    if (signal) {
      signal.addEventListener('abort', () => {
        if (awaitingResponse.delete(id)) {
          reject(signal.reason || new DOMException('The operation was aborted.', 'AbortError'));
        }
      });
    }
  });
});

// XMLHttpRequest (XHR Standard), asynchronous only.
const progressEvent = (type, loaded, total) => {
  if (typeof ProgressEvent === 'function') {
    return new ProgressEvent(type, { lengthComputable: total > 0, loaded, total });
  }
  const event = new Event(type);
  Object.defineProperties(event, {
    lengthComputable: { value: total > 0 },
    loaded: { value: loaded },
    total: { value: total },
  });
  return event;
};

class XMLHttpRequestEventTarget extends EventTarget {}
const XHR_EVENTS = ['loadstart', 'progress', 'abort', 'error', 'load', 'timeout', 'loadend'];
for (const type of XHR_EVENTS) {
  XMLHttpRequestEventTarget.prototype['on' + type] = null;
}

const kXhr = Symbol('xhr');
const fireXhr = (target, event) => {
  const handler = target['on' + event.type];
  if (typeof handler === 'function') {
    try {
      handler.call(target, event);
    } catch (error) {
      report(error);
    }
  }
  target.dispatchEvent(event);
};

class XMLHttpRequest extends XMLHttpRequestEventTarget {
  constructor() {
    super();
    this[kXhr] = {
      state: 0,
      method: 'GET',
      url: '',
      headers: [],
      id: 0,
      status: 0,
      responseUrl: '',
      responseHeaders: [],
      text: '',
      timer: undefined,
    };
    this.onreadystatechange = null;
    this.responseType = '';
    this.timeout = 0;
    this.withCredentials = false;
    this.upload = new XMLHttpRequestEventTarget();
  }

  get readyState() {
    return this[kXhr].state;
  }
  get status() {
    return this[kXhr].status;
  }
  get statusText() {
    return this[kXhr].state >= 2 ? STATUS_TEXT[this[kXhr].status] || '' : '';
  }
  get responseURL() {
    return this[kXhr].responseUrl;
  }
  get responseText() {
    if (this.responseType !== '' && this.responseType !== 'text') {
      throw new DOMException('responseText is only for text responses', 'InvalidStateError');
    }
    return this[kXhr].state >= 3 ? this[kXhr].text : '';
  }
  get responseXML() {
    return null;
  }
  get response() {
    const xhr = this[kXhr];
    if (xhr.state !== 4) return this.responseType === '' || this.responseType === 'text' ? '' : null;
    switch (this.responseType) {
      case '':
      case 'text':
        return xhr.text;
      case 'json':
        try {
          return JSON.parse(xhr.text);
        } catch (_) {
          return null;
        }
      case 'arraybuffer':
        return new TextEncoder().encode(xhr.text).buffer;
      case 'blob':
        return typeof Blob === 'function' ? new Blob([xhr.text]) : null;
      default:
        return null;
    }
  }

  #changeState(state) {
    this[kXhr].state = state;
    fireXhr(this, new Event('readystatechange'));
  }

  open(method, url, async = true) {
    const xhr = this[kXhr];
    if (async === false) {
      throw new DOMException('synchronous requests are not supported', 'InvalidAccessError');
    }
    if (xhr.id) awaitingResponse.delete(xhr.id);
    xhr.method = String(method).toUpperCase();
    xhr.url = resolveAddress(url);
    xhr.headers = [];
    xhr.id = 0;
    xhr.status = 0;
    xhr.responseUrl = '';
    xhr.responseHeaders = [];
    xhr.text = '';
    this.#changeState(1);
  }

  setRequestHeader(name, value) {
    if (this[kXhr].state !== 1 || this[kXhr].id) {
      throw new DOMException('the request is not open', 'InvalidStateError');
    }
    this[kXhr].headers.push([String(name), String(value)]);
  }

  send(body = null) {
    const xhr = this[kXhr];
    if (xhr.state !== 1 || xhr.id) {
      throw new DOMException('the request is not open', 'InvalidStateError');
    }
    const pairs = xhr.headers.slice();
    const text = xhr.method === 'GET' || xhr.method === 'HEAD' ? null : requestBody(body, pairs);
    xhr.id = nativeFetch(xhr.method, xhr.url, headerLines(pairs), text);
    fireXhr(this, progressEvent('loadstart', 0, 0));
    if (this.timeout > 0) {
      xhr.timer = setTimeout(() => {
        if (awaitingResponse.delete(xhr.id)) this.#finish('timeout');
      }, this.timeout);
    }
    awaitingResponse.set(xhr.id, {
      succeed: (answer) => {
        clearTimeout(xhr.timer);
        xhr.status = answer.status;
        xhr.responseUrl = answer.url;
        xhr.responseHeaders = answer.headers;
        this.#changeState(2);
        xhr.text = answer.body;
        this.#changeState(3);
        const size = xhr.text.length;
        fireXhr(this, progressEvent('progress', size, size));
        this.#changeState(4);
        fireXhr(this, progressEvent('load', size, size));
        fireXhr(this, progressEvent('loadend', size, size));
      },
      fail: () => {
        clearTimeout(xhr.timer);
        this.#finish('error');
      },
    });
  }

  // The request ended without a response: `error`, `abort` or `timeout`.
  #finish(type) {
    const xhr = this[kXhr];
    xhr.status = 0;
    xhr.text = '';
    xhr.responseHeaders = [];
    this.#changeState(4);
    fireXhr(this, progressEvent(type, 0, 0));
    fireXhr(this, progressEvent('loadend', 0, 0));
  }

  abort() {
    const xhr = this[kXhr];
    if (xhr.id && awaitingResponse.delete(xhr.id)) {
      clearTimeout(xhr.timer);
      this.#finish('abort');
      xhr.state = 0;
    }
  }

  getResponseHeader(name) {
    const wanted = String(name).toLowerCase();
    const values = this[kXhr].responseHeaders
      .filter(([key]) => key.toLowerCase() === wanted)
      .map(([, value]) => value);
    return values.length ? values.join(', ') : null;
  }

  getAllResponseHeaders() {
    return this[kXhr].responseHeaders.map(([name, value]) => name.toLowerCase() + ': ' + value + '\r\n').join('');
  }

  overrideMimeType() {}
}
const XHR_STATES = { UNSENT: 0, OPENED: 1, HEADERS_RECEIVED: 2, LOADING: 3, DONE: 4 };
for (const [name, value] of Object.entries(XHR_STATES)) {
  Object.defineProperty(XMLHttpRequest, name, { value, enumerable: true });
  Object.defineProperty(XMLHttpRequest.prototype, name, { value, enumerable: true });
}
defineGlobal('XMLHttpRequestEventTarget', XMLHttpRequestEventTarget);
defineGlobal('XMLHttpRequest', XMLHttpRequest);

// Scripts that script inserts (HTML §4.12.1.1 "prepare the script element"
// for a script that is not parser-inserted). The parser runs its own; one a
// page makes and connects runs when it is connected — at once when it
// carries its text, once fetched when it names a file, after which it hears
// `load`, or `error` when the fetch failed. A chunk loader (webpack, Next,
// turbopack), an analytics snippet and a JSONP call are all this.
//
// Inserted external scripts all run as soon as they arrive, which is what
// one without `async=false` does; the ordered queue `async=false` asks for is
// not kept.
const nativeFetchScript = take(Document, '__fetchScript');
const nativeClearCurrentScript = take(Document, '__clearCurrentScript');
const nativeScriptStarted = take(Element.prototype, '__scriptStarted');
const nativeBecomeCurrentScript = take(Element.prototype, '__becomeCurrentScript');

const CLASSIC_SCRIPT_TYPES = new Set([
  '',
  'text/javascript',
  'application/javascript',
  'application/ecmascript',
  'application/x-ecmascript',
  'application/x-javascript',
  'text/ecmascript',
  'text/javascript1.0',
  'text/javascript1.1',
  'text/javascript1.2',
  'text/javascript1.3',
  'text/javascript1.4',
  'text/javascript1.5',
  'text/jscript',
  'text/livescript',
  'text/x-ecmascript',
  'text/x-javascript',
]);
const scriptKind = (script) => {
  const type = script.getAttribute('type');
  const kind = (type === null ? script.getAttribute('language') === null ? '' : 'text/' + script.getAttribute('language') : type)
    .trim()
    .toLowerCase();
  if (CLASSIC_SCRIPT_TYPES.has(kind)) return 'classic';
  if (kind === 'module') return 'module';
  return null;
};
const reportScriptError = (error, url) => {
  const message =
    error && typeof error === 'object' && 'message' in error
      ? 'Uncaught ' + (error.name || 'Error') + ': ' + error.message
      : 'Uncaught ' + String(error);
  __otlyraReportError(message, url, error);
};
const fireScriptEvent = (script, type) => {
  try {
    script.dispatchEvent(new Event(type));
  } catch (error) {
    console.error(error);
  }
};
// Run a classic script's text as a script of its own: in the global scope,
// with `document.currentScript` the element.
const runClassicScript = (script, source, url) => {
  nativeBecomeCurrentScript.call(script);
  try {
    (0, eval)(source + '\n//# sourceURL=' + url);
  } catch (error) {
    reportScriptError(error, url);
  } finally {
    nativeClearCurrentScript();
  }
};
const prepareInsertedScript = (script) => {
  if (!script.isConnected || nativeScriptStarted.call(script, false)) return;
  const kind = scriptKind(script);
  if (kind === null) return;
  const src = script.getAttribute('src');
  // A script with neither a file nor text waits: its text may come later.
  if (src === null && !script.text) return;
  nativeScriptStarted.call(script, true);
  if (kind === 'classic' && script.hasAttribute('nomodule')) return;
  if (src === null) {
    if (kind === 'classic') runClassicScript(script, script.text, String(location.href));
    return;
  }
  const url = script.src;
  if (!url) {
    queueMicrotask(() => fireScriptEvent(script, 'error'));
    return;
  }
  if (kind === 'module') {
    import(url).then(
      () => fireScriptEvent(script, 'load'),
      (error) => {
        reportScriptError(error, url);
        fireScriptEvent(script, 'error');
      },
    );
    return;
  }
  const id = nativeFetchScript(url);
  awaitingResponse.set(id, {
    succeed(response) {
      if (response.status < 200 || response.status >= 300) {
        fireScriptEvent(script, 'error');
        return;
      }
      runClassicScript(script, response.body, response.url || url);
      fireScriptEvent(script, 'load');
    },
    fail() {
      fireScriptEvent(script, 'error');
    },
  });
};
// What an insertion may have connected: the nodes handed to it, or a
// fragment's children, taken before it empties.
const insertedCandidates = (nodes) => {
  const found = [];
  for (const node of nodes) {
    if (!node || typeof node !== 'object' || typeof node.nodeType !== 'number') continue;
    if (node.nodeType === 11) found.push(...Array.from(node.childNodes));
    else if (node.nodeType === 1) found.push(node);
  }
  return found;
};
const startInsertedScripts = (candidates) => {
  for (const node of candidates) {
    if (node.nodeType !== 1 || !node.isConnected) continue;
    if (node.localName === 'script') {
      prepareInsertedScript(node);
    } else if (node.firstElementChild) {
      for (const script of Array.from(node.getElementsByTagName('script'))) {
        prepareInsertedScript(script);
      }
    }
  }
};
for (const [name, at] of [['appendChild', 0], ['insertBefore', 0], ['replaceChild', 0]]) {
  const native = Node.prototype[name];
  if (typeof native !== 'function') continue;
  method(Node.prototype, name, {
    [name](...args) {
      const candidates = insertedCandidates([args[at]]);
      const result = native.apply(this, args);
      if (candidates.length > 0) startInsertedScripts(candidates);
      return result;
    },
  }[name]);
}
if (typeof Element.prototype.insertAdjacentElement === 'function') {
  const native = Element.prototype.insertAdjacentElement;
  method(Element.prototype, 'insertAdjacentElement', function insertAdjacentElement(where, element) {
    const result = native.call(this, where, element);
    startInsertedScripts(insertedCandidates([element]));
    return result;
  });
}
// A connected script given its text or its `src` after the fact is
// prepared then (HTML §4.12.1.1: "becomes connected", "src set").
for (const property of ['text', 'textContent', 'src']) {
  for (const proto of [HTMLScriptElement.prototype, Node.prototype]) {
    if (property !== 'textContent' && proto === Node.prototype) continue;
    if (property === 'textContent' && proto !== Node.prototype) continue;
    const descriptor = Object.getOwnPropertyDescriptor(proto, property);
    if (!descriptor || !descriptor.set) continue;
    Object.defineProperty(proto, property, {
      ...descriptor,
      set(value) {
        descriptor.set.call(this, value);
        if (this.localName === 'script' && this.isConnected) prepareInsertedScript(this);
      },
    });
  }
}
