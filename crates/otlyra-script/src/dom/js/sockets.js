// ---------------------------------------------------------------- Sockets
// WebSocket (HTML §9.3) and EventSource (§9.2) over the browser's network:
// the connection runs on a thread of the browser's (`sockets.rs`), and what
// it reports is fired here as `open`, `message`, `error` and `close`, a turn
// after it arrived. With no network to open one on, a connection fails as one
// to an unreachable server does: `error`, then `close`, a task later.

const nativeSocketOpen = take(Document, '__socketOpen');
const nativeSocketSend = take(Document, '__socketSend');
const nativeSocketClose = take(Document, '__socketClose');
const nativeSocketNextEvent = take(Document, '__socketNextEvent');
const liveSockets = new Map();
const kSocketEvent = Symbol('socketEvent');

class WebSocket extends EventTarget {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;
  #url;
  #state = 0;
  #id = null;
  #protocol = '';
  #buffered = 0;
  binaryType = 'blob';

  constructor(url, protocols = []) {
    super();
    if (arguments.length < 1) throw new TypeError("Failed to construct 'WebSocket': 1 argument required");
    let parsed;
    try {
      parsed = new URL(String(url), documentBase());
    } catch (_) {
      throw new DOMException("Failed to construct 'WebSocket': The URL '" + url + "' is invalid.", 'SyntaxError');
    }
    if (parsed.protocol === 'http:') parsed.protocol = 'ws:';
    if (parsed.protocol === 'https:') parsed.protocol = 'wss:';
    if (parsed.protocol !== 'ws:' && parsed.protocol !== 'wss:') {
      throw new DOMException("Failed to construct 'WebSocket': The URL's scheme must be either 'ws' or 'wss'.", 'SyntaxError');
    }
    if (parsed.hash) throw new DOMException("Failed to construct 'WebSocket': The URL contains a fragment identifier.", 'SyntaxError');
    this.#url = parsed.href;
    const offered = (typeof protocols === 'string' ? [protocols] : [...protocols]).map(String);
    if (new Set(offered).size !== offered.length) {
      throw new DOMException("Failed to construct 'WebSocket': The subprotocol is duplicated.", 'SyntaxError');
    }
    const id = nativeSocketOpen(0, this.#url, offered.join('\n'), location.origin);
    if (id === null || id === undefined) {
      setTimeout(() => this.#fail(), 0);
      return;
    }
    this.#id = Number(id);
    liveSockets.set(this.#id, this);
  }
  #fail() {
    if (this.#state === 3) return;
    this.#state = 3;
    this.dispatchEvent(new Event('error'));
    this.dispatchEvent(new CloseEvent('close', { code: 1006, reason: '', wasClean: false }));
  }
  get url() { return this.#url; }
  get readyState() { return this.#state; }
  get bufferedAmount() { return this.#buffered; }
  get extensions() { return ''; }
  get protocol() { return this.#protocol; }
  send(data) {
    if (this.#state === 0) throw new DOMException("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.", 'InvalidStateError');
    if (this.#state !== 1) return;
    if (typeof data === 'string') {
      nativeSocketSend(this.#id, data, false);
    } else if (data instanceof ArrayBuffer || ArrayBuffer.isView(data)) {
      const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
      nativeSocketSend(this.#id, bytesToText(bytes), true);
    } else if (data instanceof Blob) {
      data.arrayBuffer().then((buffer) => {
        if (this.#state === 1) nativeSocketSend(this.#id, bytesToText(new Uint8Array(buffer)), true);
      });
    } else {
      nativeSocketSend(this.#id, String(data), false);
    }
  }
  close(code, reason = '') {
    if (code !== undefined && code !== 1000 && (code < 3000 || code > 4999)) {
      throw new DOMException(`Failed to execute 'close' on 'WebSocket': The code must be either 1000, or between 3000 and 4999. ${code} is neither.`, 'InvalidAccessError');
    }
    if (this.#state === 2 || this.#state === 3) return;
    this.#state = 2;
    if (this.#id !== null) nativeSocketClose(this.#id, code ?? 1000, String(reason));
  }
  // What the connection reported, from `__otlyraSocketDeliver`.
  [kSocketEvent](kind, a, b, c) {
    if (kind === 'open') {
      if (this.#state !== 0) return;
      this.#state = 1;
      this.#protocol = a;
      this.dispatchEvent(new Event('open'));
    } else if (kind === 'text' || kind === 'binary') {
      if (this.#state !== 1) return;
      let data = a;
      if (kind === 'binary') {
        const bytes = textToBytes(a);
        data = this.binaryType === 'arraybuffer' ? bytes.buffer : new Blob([bytes]);
      }
      this.dispatchEvent(new MessageEvent('message', { data, origin: new URL(this.#url).origin }));
    } else if (kind === 'error') {
      this.dispatchEvent(new Event('error'));
    } else if (kind === 'close') {
      liveSockets.delete(this.#id);
      this.#state = 3;
      this.dispatchEvent(new CloseEvent('close', { code: Number(a), reason: b, wasClean: c === 'true' }));
    }
  }
}
Object.assign(WebSocket.prototype, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });
for (const type of ['open', 'message', 'error', 'close']) defineHandler(WebSocket.prototype, type);
defineGlobal('WebSocket', WebSocket);

class EventSource extends EventTarget {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  #url;
  #state = 0;
  #credentials;
  #id = null;

  constructor(url, init = {}) {
    super();
    try {
      this.#url = new URL(String(url), documentBase()).href;
    } catch (_) {
      throw new DOMException("Failed to construct 'EventSource': Cannot open an EventSource to '" + url + "'. The URL is invalid.", 'SyntaxError');
    }
    this.#credentials = Boolean(init && init.withCredentials);
    const id = nativeSocketOpen(1, this.#url, '', location.origin);
    if (id === null || id === undefined) {
      setTimeout(() => {
        if (this.#state === 2) return;
        this.#state = 2;
        this.dispatchEvent(new Event('error'));
      }, 0);
      return;
    }
    this.#id = Number(id);
    liveSockets.set(this.#id, this);
  }
  get url() { return this.#url; }
  get readyState() { return this.#state; }
  get withCredentials() { return this.#credentials; }
  close() {
    if (this.#state === 2) return;
    this.#state = 2;
    if (this.#id !== null) {
      nativeSocketClose(this.#id, 1000, '');
      liveSockets.delete(this.#id);
    }
  }
  [kSocketEvent](kind, a, b, c) {
    if (this.#state === 2) return;
    if (kind === 'open') {
      this.#state = 1;
      this.dispatchEvent(new Event('open'));
    } else if (kind === 'event') {
      this.dispatchEvent(new MessageEvent(a, { data: b, lastEventId: c, origin: new URL(this.#url).origin }));
    } else if (kind === 'error') {
      // Lost, and reconnecting.
      this.#state = 0;
      this.dispatchEvent(new Event('error'));
    } else if (kind === 'close') {
      this.#state = 2;
      liveSockets.delete(this.#id);
      this.dispatchEvent(new Event('error'));
    }
  }
}
Object.assign(EventSource.prototype, { CONNECTING: 0, OPEN: 1, CLOSED: 2 });
for (const type of ['open', 'message', 'error']) defineHandler(EventSource.prototype, type);
defineGlobal('EventSource', EventSource);

// What the connections reported since the last turn, fired at their objects.
defineGlobal('__otlyraSocketDeliver', () => {
  let item;
  while ((item = nativeSocketNextEvent()) !== null && item !== undefined) {
    const [id, kind, a, b, c] = item;
    const socket = liveSockets.get(Number(id));
    if (socket) socket[kSocketEvent](kind, a, b, c);
  }
});
