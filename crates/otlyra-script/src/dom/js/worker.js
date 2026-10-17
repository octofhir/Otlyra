// ---------------------------------------------------------------- Dedicated workers
// `new Worker(url, options)` (HTML §10.2.6). The worker itself runs in an
// isolate on a thread of its own (`worker.rs`); this is the page's end of it:
// an event target its messages and errors are fired at, and `postMessage`
// and `terminate` for talking to it. Messages cross as `clone.js` text.

const nativeWorkerStart = take(Document, '__workerStart');
const nativeWorkerPost = take(Document, '__workerPost');
const nativeWorkerTerminate = take(Document, '__workerTerminate');
const nativeWorkerNextEvent = take(Document, '__workerNextEvent');
const kWorkerId = Symbol('workerId');
const kWorkerQueue = Symbol('workerQueue');
const liveWorkers = new Map();

class Worker extends EventTarget {
  constructor(url, options = {}) {
    super();
    if (url === undefined) {
      throw new TypeError("Failed to construct 'Worker': 1 argument required, but only 0 present.");
    }
    const address = String(url instanceof URL ? url.href : url);
    const resolved = address.startsWith('blob:') ? address : new URL(address, document.baseURI).href;
    const module = options?.type === 'module';
    this.onmessage = null;
    this.onerror = null;
    this.onmessageerror = null;
    this[kWorkerId] = null;
    // Messages posted before the worker has been started wait here: an object
    // URL's script is read from its Blob, which takes a turn.
    this[kWorkerQueue] = [];
    const start = (source) => {
      if (this[kWorkerQueue] === null) return;
      const id = nativeWorkerStart(resolved, module, source);
      if (id === null || id === undefined) {
        queueMicrotask(() => this.dispatchEvent(new Event('error')));
        return;
      }
      this[kWorkerId] = id;
      liveWorkers.set(id, this);
      for (const data of this[kWorkerQueue]) nativeWorkerPost(id, data);
      this[kWorkerQueue] = [];
    };
    if (resolved.startsWith('blob:')) {
      const blob = objectUrls.get(resolved.split('#')[0]);
      if (!blob) {
        queueMicrotask(() => this.dispatchEvent(new Event('error')));
      } else {
        blob.text().then(start, () => this.dispatchEvent(new Event('error')));
      }
    } else {
      start(null);
    }
  }
  postMessage(message, transfer) {
    const data = cloneSerialize(message);
    if (this[kWorkerQueue] !== null && this[kWorkerId] === null) this[kWorkerQueue].push(data);
    else if (this[kWorkerId] !== null) nativeWorkerPost(this[kWorkerId], data);
  }
  terminate() {
    if (this[kWorkerId] !== null) {
      nativeWorkerTerminate(this[kWorkerId]);
      liveWorkers.delete(this[kWorkerId]);
    }
    this[kWorkerQueue] = null;
    this[kWorkerId] = null;
  }
}

// What the workers sent since the last turn, fired at their Worker objects.
defineGlobal('__otlyraWorkerDeliver', () => {
  let item;
  while ((item = nativeWorkerNextEvent()) !== null && item !== undefined) {
    const [id, kind, data] = item;
    const worker = liveWorkers.get(Number(id));
    if (!worker) continue;
    if (kind === 'message') {
      let value;
      try {
        value = cloneDeserialize(data);
      } catch (_) {
        const event = new MessageEvent('messageerror', { data: null });
        if (typeof worker.onmessageerror === 'function') worker.onmessageerror(event);
        worker.dispatchEvent(event);
        continue;
      }
      const event = new MessageEvent('message', { data: value });
      if (typeof worker.onmessage === 'function') worker.onmessage(event);
      worker.dispatchEvent(event);
    } else {
      const event = typeof ErrorEvent === 'function'
        ? new ErrorEvent('error', { message: data, cancelable: true })
        : Object.assign(new Event('error', { cancelable: true }), { message: data });
      if (typeof worker.onerror === 'function') worker.onerror(event);
      worker.dispatchEvent(event);
      if (!event.defaultPrevented) console.error(`Uncaught (in worker) ${data}`);
    }
  }
});

defineGlobal('Worker', Worker);
