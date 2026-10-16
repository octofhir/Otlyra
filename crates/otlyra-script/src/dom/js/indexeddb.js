// ------------------------------------------------------------- IndexedDB
// IndexedDB 3.0, kept in memory for as long as the document is: a page reads
// back what it wrote, and nothing outlives it — what a private window gives.
// Pages open a database at load whether or not they need one (an error
// tracker's queue, a framework's cache), and one that is missing stops the
// script that asked.
//
// The model is the specification's, kept small:
// - A database is named object stores of records sorted by key; an index is
//   recomputed from its store when it is asked, which is linear in the store
//   and plenty for what a page keeps in a tab.
// - Values are stored and handed back as structured clones, so a record never
//   changes under a page that keeps a reference to what it put.
// - A transaction runs its requests in order, one task each, firing success
//   or error at the request; it commits once a task passes with nothing left
//   to run, and an abort puts every store it wrote back as it found it.

const IDB_EVENT_TYPES = {
  request: ['success', 'error'],
  open: ['success', 'error', 'upgradeneeded', 'blocked'],
  transaction: ['complete', 'abort', 'error'],
  database: ['abort', 'close', 'error', 'versionchange'],
};

const idbError = (name, message) => new DOMException(message, name);
const idbTask = (callback) => setTimeout(callback, 0);

// Keys (§2.6, §7.1 "compare two keys"): numbers, dates, strings, binary and
// arrays of keys, in that order from least.
const KEY_ORDER = { number: 0, date: 1, string: 2, binary: 3, array: 4 };
const keyType = (key) => {
  if (typeof key === 'number') return Number.isNaN(key) ? null : 'number';
  if (typeof key === 'string') return 'string';
  if (key instanceof Date) return Number.isNaN(key.getTime()) ? null : 'date';
  if (key instanceof ArrayBuffer || ArrayBuffer.isView(key)) return 'binary';
  if (Array.isArray(key)) return 'array';
  return null;
};
// §7.1 "convert a value to a key": a copy, so a page changing the array or
// the date it passed changes nothing here.
const toKey = (value, seen = new Set()) => {
  const type = keyType(value);
  if (type === null) return undefined;
  if (type === 'date') return new Date(value.getTime());
  if (type === 'binary') {
    const bytes = value instanceof ArrayBuffer ? new Uint8Array(value) : new Uint8Array(value.buffer, value.byteOffset, value.byteLength);
    return bytes.slice().buffer;
  }
  if (type === 'array') {
    if (seen.has(value)) return undefined;
    seen.add(value);
    const out = [];
    for (let index = 0; index < value.length; index++) {
      const item = toKey(value[index], seen);
      if (item === undefined) return undefined;
      out.push(item);
    }
    seen.delete(value);
    return out;
  }
  return value;
};
const requireKey = (value) => {
  const key = toKey(value);
  if (key === undefined) throw idbError('DataError', 'the parameter is not a valid key');
  return key;
};
const compareKeys = (a, b) => {
  const ta = keyType(a);
  const tb = keyType(b);
  if (ta !== tb) return KEY_ORDER[ta] < KEY_ORDER[tb] ? -1 : 1;
  switch (ta) {
    case 'number':
      return a < b ? -1 : a > b ? 1 : 0;
    case 'date':
      return compareKeys(a.getTime(), b.getTime());
    case 'string':
      return a < b ? -1 : a > b ? 1 : 0;
    case 'binary': {
      const x = new Uint8Array(a);
      const y = new Uint8Array(b);
      for (let index = 0; index < Math.min(x.length, y.length); index++) {
        if (x[index] !== y[index]) return x[index] < y[index] ? -1 : 1;
      }
      return compareKeys(x.length, y.length);
    }
    default: {
      for (let index = 0; index < Math.min(a.length, b.length); index++) {
        const order = compareKeys(a[index], b[index]);
        if (order !== 0) return order;
      }
      return compareKeys(a.length, b.length);
    }
  }
};

// §7.5 key paths: a string of dotted identifiers, or a list of them.
const validKeyPath = (path) =>
  typeof path === 'string'
    ? path === '' || path.split('.').every((part) => /^[$_\p{ID_Start}][$‌‍\p{ID_Continue}]*$/u.test(part))
    : Array.isArray(path) && path.length > 0 && path.every((part) => typeof part === 'string' && validKeyPath(part));
const evaluateKeyPath = (value, path) => {
  if (Array.isArray(path)) {
    const out = [];
    for (const part of path) {
      const key = evaluateKeyPath(value, part);
      if (key === undefined) return undefined;
      out.push(key);
    }
    return out;
  }
  if (path === '') return toKey(value);
  let current = value;
  for (const part of path.split('.')) {
    if (current === null || current === undefined) return undefined;
    if (typeof current === 'string' && part === 'length') current = current.length;
    else if (typeof current !== 'object') return undefined;
    else if (!(part in Object(current))) return undefined;
    else current = current[part];
  }
  return toKey(current);
};
const injectKey = (value, path, key) => {
  const parts = path.split('.');
  let current = value;
  for (const part of parts.slice(0, -1)) {
    if (current[part] === undefined) current[part] = {};
    current = current[part];
  }
  current[parts[parts.length - 1]] = key;
};

class IDBKeyRange {
  #lower;
  #upper;
  #lowerOpen;
  #upperOpen;

  constructor(lower, upper, lowerOpen, upperOpen, token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
    this.#lower = lower;
    this.#upper = upper;
    this.#lowerOpen = lowerOpen;
    this.#upperOpen = upperOpen;
  }
  get lower() {
    return this.#lower;
  }
  get upper() {
    return this.#upper;
  }
  get lowerOpen() {
    return this.#lowerOpen;
  }
  get upperOpen() {
    return this.#upperOpen;
  }
  includes(key) {
    key = requireKey(key);
    if (this.#lower !== undefined) {
      const order = compareKeys(this.#lower, key);
      if (order > 0 || (order === 0 && this.#lowerOpen)) return false;
    }
    if (this.#upper !== undefined) {
      const order = compareKeys(this.#upper, key);
      if (order < 0 || (order === 0 && this.#upperOpen)) return false;
    }
    return true;
  }
  static only(value) {
    const key = requireKey(value);
    return new IDBKeyRange(key, key, false, false, kIdbInternal);
  }
  static lowerBound(lower, open = false) {
    return new IDBKeyRange(requireKey(lower), undefined, Boolean(open), true, kIdbInternal);
  }
  static upperBound(upper, open = false) {
    return new IDBKeyRange(undefined, requireKey(upper), true, Boolean(open), kIdbInternal);
  }
  static bound(lower, upper, lowerOpen = false, upperOpen = false) {
    const low = requireKey(lower);
    const high = requireKey(upper);
    const order = compareKeys(low, high);
    if (order > 0 || (order === 0 && (lowerOpen || upperOpen))) {
      throw idbError('DataError', 'the lower bound is greater than the upper bound');
    }
    return new IDBKeyRange(low, high, Boolean(lowerOpen), Boolean(upperOpen), kIdbInternal);
  }
}
const kIdbInternal = Symbol('idbInternal');
// §4.7.4 "convert a value to a key range": a key range, a key, or nothing.
const toKeyRange = (query, nullDisallowed = false) => {
  if (query instanceof IDBKeyRange) return query;
  if (query === undefined || query === null) {
    if (nullDisallowed) throw idbError('DataError', 'a key or key range is required');
    return null;
  }
  return IDBKeyRange.only(query);
};
const inRange = (range, key) => range === null || range.includes(key);

// A list of names that answers `contains` (DOMStringList).
const nameList = (names) => {
  const list = [...names].sort();
  Object.defineProperty(list, 'contains', { value: (name) => list.includes(String(name)) });
  Object.defineProperty(list, 'item', { value: (index) => list[index] ?? null });
  return list;
};

// What the document keeps: database name → { version, stores }, where a
// store is { name, keyPath, autoIncrement, current, records, indexes }.
const idbDatabases = new Map();
const idbConnections = new Map();

class IDBRequest extends EventTarget {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
    super();
    this[kIdbRequest] = { result: undefined, error: null, done: false, source: null, transaction: null };
  }
  get result() {
    const state = this[kIdbRequest];
    if (!state.done) throw idbError('InvalidStateError', 'the request has not finished');
    return state.result;
  }
  get error() {
    const state = this[kIdbRequest];
    if (!state.done) throw idbError('InvalidStateError', 'the request has not finished');
    return state.error;
  }
  get source() {
    return this[kIdbRequest].source;
  }
  get transaction() {
    return this[kIdbRequest].transaction;
  }
  get readyState() {
    return this[kIdbRequest].done ? 'done' : 'pending';
  }
}
const kIdbRequest = Symbol('idbRequest');
class IDBOpenDBRequest extends IDBRequest {}
for (const type of IDB_EVENT_TYPES.request) defineHandler(IDBRequest.prototype, type);
for (const type of ['upgradeneeded', 'blocked']) defineHandler(IDBOpenDBRequest.prototype, type);

class IDBVersionChangeEvent extends Event {
  #oldVersion;
  #newVersion;
  constructor(type, init = {}) {
    super(type, init);
    this.#oldVersion = Number(init.oldVersion ?? 0);
    this.#newVersion = init.newVersion === undefined || init.newVersion === null ? null : Number(init.newVersion);
  }
  get oldVersion() {
    return this.#oldVersion;
  }
  get newVersion() {
    return this.#newVersion;
  }
}

// Fire `type` at `target`, which bubbles to its transaction and database
// when `bubbles`; answers whether no listener cancelled it.
const idbFire = (target, type, { bubbles = false, cancelable = false, event } = {}) => {
  const fired = event || new Event(type, { bubbles, cancelable });
  try {
    return target.dispatchEvent(fired);
  } catch (error) {
    reportError(error);
    return true;
  }
};

class IDBDatabase extends EventTarget {
  constructor(token, name, data) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
    super();
    this[kIdbDatabase] = { name, data, closed: false, closePending: false, upgrade: null };
  }
  get name() {
    return this[kIdbDatabase].name;
  }
  get version() {
    return this[kIdbDatabase].data.version;
  }
  get objectStoreNames() {
    return nameList(this[kIdbDatabase].data.stores.keys());
  }
  createObjectStore(name, options = {}) {
    const state = this[kIdbDatabase];
    const upgrade = state.upgrade;
    if (!upgrade) throw idbError('InvalidStateError', 'object stores are created in an upgrade');
    if (!upgrade.isActive()) throw idbError('TransactionInactiveError', 'the upgrade transaction is not active');
    name = String(name);
    const keyPath = options?.keyPath ?? null;
    const autoIncrement = Boolean(options?.autoIncrement);
    if (keyPath !== null && !validKeyPath(keyPath)) throw idbError('SyntaxError', 'invalid key path');
    if (state.data.stores.has(name)) throw idbError('ConstraintError', 'an object store named ' + name + ' exists');
    if (autoIncrement && (keyPath === '' || Array.isArray(keyPath))) {
      throw idbError('InvalidAccessError', 'an auto-incrementing store needs a single non-empty key path');
    }
    const store = { name, keyPath, autoIncrement, current: 1, records: [], indexes: new Map() };
    upgrade.touch(store);
    state.data.stores.set(name, store);
    return upgrade.transaction.objectStore(name);
  }
  deleteObjectStore(name) {
    const upgrade = this[kIdbDatabase].upgrade;
    if (!upgrade) throw idbError('InvalidStateError', 'object stores are deleted in an upgrade');
    if (!upgrade.isActive()) throw idbError('TransactionInactiveError', 'the upgrade transaction is not active');
    const stores = this[kIdbDatabase].data.stores;
    if (!stores.has(String(name))) throw idbError('NotFoundError', 'no object store named ' + name);
    upgrade.touch(stores.get(String(name)));
    stores.delete(String(name));
  }
  transaction(storeNames, mode = 'readonly', _options) {
    const state = this[kIdbDatabase];
    if (state.closePending || state.closed) throw idbError('InvalidStateError', 'the connection is closing');
    if (state.upgrade && state.upgrade.isRunning()) {
      throw idbError('InvalidStateError', 'an upgrade transaction is running');
    }
    const names = typeof storeNames === 'string' ? [storeNames] : [...storeNames].map(String);
    if (names.length === 0) throw idbError('InvalidAccessError', 'a transaction needs at least one store');
    for (const name of names) {
      if (!state.data.stores.has(name)) throw idbError('NotFoundError', 'no object store named ' + name);
    }
    if (mode !== 'readonly' && mode !== 'readwrite') throw new TypeError('invalid transaction mode ' + mode);
    return makeTransaction(this, [...new Set(names)], mode);
  }
  close() {
    const state = this[kIdbDatabase];
    state.closePending = true;
    state.closed = true;
    idbConnections.get(state.name)?.delete(this);
  }
}
const kIdbDatabase = Symbol('idbDatabase');
for (const type of IDB_EVENT_TYPES.database) defineHandler(IDBDatabase.prototype, type);

class IDBTransaction extends EventTarget {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
    super();
  }
  get db() {
    return this[kIdbTransaction].db;
  }
  get mode() {
    return this[kIdbTransaction].mode;
  }
  get durability() {
    return 'default';
  }
  get error() {
    return this[kIdbTransaction].error;
  }
  get objectStoreNames() {
    return nameList(this[kIdbTransaction].scope());
  }
  objectStore(name) {
    const state = this[kIdbTransaction];
    if (state.finished) throw idbError('InvalidStateError', 'the transaction has finished');
    name = String(name);
    if (!state.scope().includes(name)) throw idbError('NotFoundError', 'no object store named ' + name + ' in this transaction');
    if (!state.handles.has(name)) state.handles.set(name, makeStore(this, name));
    return state.handles.get(name);
  }
  commit() {
    const state = this[kIdbTransaction];
    if (state.finished) throw idbError('InvalidStateError', 'the transaction has finished');
    state.commitRequested = true;
  }
  abort() {
    const state = this[kIdbTransaction];
    if (state.finished) throw idbError('InvalidStateError', 'the transaction has finished');
    state.abort(null);
  }
}
const kIdbTransaction = Symbol('idbTransaction');
for (const type of IDB_EVENT_TYPES.transaction) defineHandler(IDBTransaction.prototype, type);

const makeTransaction = (db, names, mode, onFinish) => {
  const transaction = new IDBTransaction(kIdbInternal);
  const data = db[kIdbDatabase].data;
  const queue = [];
  const saved = new Map();
  const state = {
    db,
    mode,
    error: null,
    finished: false,
    commitRequested: false,
    handles: new Map(),
    scope: () => (mode === 'versionchange' ? [...data.stores.keys()] : names),
    // Active from creation until it finishes. The specification makes it
    // inactive between its tasks, but the promise wrappers pages use (idb,
    // Dexie) make their next request from a reaction that, here, runs after
    // the task that fired the event has returned; a request made then is
    // queued like any other, and the transaction commits a task after the
    // last one was answered.
    isActive: () => !state.finished,
    isRunning: () => !state.finished,
    // Before the first change to a store, what to put back on abort.
    touch(store) {
      if (saved.has(store)) return;
      saved.set(store, {
        records: store.records.slice(),
        current: store.current,
        indexes: new Map(store.indexes),
        name: store.name,
      });
    },
    request(source, operation) {
      if (!state.isActive()) throw idbError('TransactionInactiveError', 'the transaction is not active');
      const request = new IDBRequest(kIdbInternal);
      request[kIdbRequest].source = source;
      request[kIdbRequest].transaction = transaction;
      eventParents.set(request, transaction);
      queue.push({ request, operation });
      schedule();
      return request;
    },
    requeue(request, operation) {
      if (!state.isActive()) throw idbError('TransactionInactiveError', 'the transaction is not active');
      queue.push({ request, operation });
      schedule();
    },
    abort(error) {
      if (state.finished) return;
      state.finished = true;
      state.error = error;
      for (const [store, before] of saved) {
        store.records = before.records;
        store.current = before.current;
        store.indexes = before.indexes;
      }
      if (mode === 'versionchange' && state.storesBefore) {
        data.stores = state.storesBefore;
        data.version = state.versionBefore;
      }
      for (const { request } of queue.splice(0)) {
        request[kIdbRequest].done = true;
        request[kIdbRequest].error = idbError('AbortError', 'the transaction was aborted');
        idbFire(request, 'error', { bubbles: true, cancelable: true });
      }
      idbTask(() => {
        idbFire(transaction, 'abort', { bubbles: true });
        onFinish?.(false);
      });
    },
  };
  state.transaction = transaction;
  transaction[kIdbTransaction] = state;
  eventParents.set(transaction, db);
  let scheduled = false;
  const schedule = () => {
    if (scheduled) return;
    scheduled = true;
    idbTask(step);
  };
  const step = () => {
    scheduled = false;
    if (state.finished) return;
    const next = queue.shift();
    if (!next) {
      state.finished = true;
      idbFire(transaction, 'complete');
      onFinish?.(true);
      return;
    }
    const { request, operation } = next;
    let result;
    let error = null;
    try {
      if (mode !== 'readonly') state.writing = true;
      result = operation();
    } catch (thrown) {
      error = thrown;
    }
    const requestState = request[kIdbRequest];
    requestState.done = true;
    if (error === null) {
      requestState.result = result;
      idbFire(request, 'success');
    } else {
      requestState.error = error;
      const unhandled = idbFire(request, 'error', { bubbles: true, cancelable: true });
      if (unhandled && !state.finished) state.abort(error);
    }
    schedule();
  };
  // A transaction with no requests still commits, a task later.
  schedule();
  return transaction;
};

const storeData = (transaction, name) => {
  const state = transaction[kIdbTransaction];
  const store = state.db[kIdbDatabase].data.stores.get(name);
  if (!store) throw idbError('InvalidStateError', 'the object store was deleted');
  return store;
};
// Where `key` is, or would go, in a store's sorted records.
const findRecord = (records, key) => {
  let low = 0;
  let high = records.length;
  while (low < high) {
    const middle = (low + high) >> 1;
    if (compareKeys(records[middle].key, key) < 0) low = middle + 1;
    else high = middle;
  }
  return { at: low, found: low < records.length && compareKeys(records[low].key, key) === 0 };
};
// The (index key, primary key, value) entries of an index, in index order.
const indexEntries = (store, index) => {
  const entries = [];
  for (const record of store.records) {
    const key = evaluateKeyPath(record.value, index.keyPath);
    if (key === undefined) continue;
    if (index.multiEntry && Array.isArray(key)) {
      const seen = [];
      for (const item of key) {
        if (keyType(item) === null || seen.some((other) => compareKeys(other, item) === 0)) continue;
        seen.push(item);
        entries.push({ key: item, primaryKey: record.key, value: record.value });
      }
    } else {
      entries.push({ key, primaryKey: record.key, value: record.value });
    }
  }
  entries.sort((a, b) => compareKeys(a.key, b.key) || compareKeys(a.primaryKey, b.primaryKey));
  return entries;
};
const checkUnique = (store, record, replacing) => {
  for (const index of store.indexes.values()) {
    if (!index.unique) continue;
    const key = evaluateKeyPath(record.value, index.keyPath);
    if (key === undefined) continue;
    const keys = index.multiEntry && Array.isArray(key) ? key : [key];
    for (const other of store.records) {
      if (replacing && compareKeys(other.key, record.key) === 0) continue;
      const otherKey = evaluateKeyPath(other.value, index.keyPath);
      if (otherKey === undefined) continue;
      const otherKeys = index.multiEntry && Array.isArray(otherKey) ? otherKey : [otherKey];
      if (keys.some((a) => otherKeys.some((b) => compareKeys(a, b) === 0))) {
        throw idbError('ConstraintError', 'a record already has this key in the unique index ' + index.name);
      }
    }
  }
};

const storeWrite = (self, value, key, noOverwrite) => {
  const { transaction, name } = self[kIdbStore];
  const state = transaction[kIdbTransaction];
  if (!state.isActive()) throw idbError('TransactionInactiveError', 'the transaction is not active');
  if (state.mode === 'readonly') throw idbError('ReadOnlyError', 'the transaction is read-only');
  const store = storeData(transaction, name);
  if (store.keyPath !== null && key !== undefined) {
    throw idbError('DataError', 'a store with a key path takes its keys from the values');
  }
  if (store.keyPath === null && key === undefined && !store.autoIncrement) {
    throw idbError('DataError', 'a store with no key path and no key generator needs a key');
  }
  const clone = structuredClone(value);
  let explicit = key === undefined ? undefined : requireKey(key);
  if (store.keyPath !== null) {
    explicit = evaluateKeyPath(clone, store.keyPath);
    if (explicit === undefined && !store.autoIncrement) {
      throw idbError('DataError', 'the value has no key at its key path');
    }
    if (explicit === undefined && (clone === null || typeof clone !== 'object')) {
      throw idbError('DataError', 'the key cannot be put into the value');
    }
  }
  return state.request(self, () => {
    state.touch(store);
    let recordKey = explicit;
    if (recordKey === undefined) {
      if (store.current > 2 ** 53) throw idbError('ConstraintError', 'the key generator is exhausted');
      recordKey = store.current++;
      if (store.keyPath !== null) injectKey(clone, store.keyPath, recordKey);
    } else if (store.autoIncrement && typeof recordKey === 'number' && recordKey >= store.current) {
      store.current = Math.floor(recordKey) + 1;
    }
    const { at, found } = findRecord(store.records, recordKey);
    if (found && noOverwrite) throw idbError('ConstraintError', 'a record with this key exists');
    const record = { key: recordKey, value: clone };
    checkUnique(store, record, found);
    if (found) store.records[at] = record;
    else store.records.splice(at, 0, record);
    return recordKey;
  });
};

const storeRead = (self, operation) => {
  const { transaction, name } = self[kIdbStore];
  const store = storeData(transaction, name);
  return transaction[kIdbTransaction].request(self, () => operation(store));
};

const indexData = (self) => {
  const { store, name } = self[kIdbIndex];
  const data = storeData(store[kIdbStore].transaction, store[kIdbStore].name);
  const index = data.indexes.get(name);
  if (!index) throw idbError('InvalidStateError', 'the index was deleted');
  return { data, index };
};
const indexRead = (self, operation) => {
  const store = self[kIdbIndex].store;
  return store[kIdbStore].transaction[kIdbTransaction].request(self, () => {
    const { data, index } = indexData(self);
    return operation(indexEntries(data, index));
  });
};

class IDBObjectStore {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
  }
  get name() {
    return this[kIdbStore].name;
  }
  get keyPath() {
    return storeData(this[kIdbStore].transaction, this[kIdbStore].name).keyPath;
  }
  get autoIncrement() {
    return storeData(this[kIdbStore].transaction, this[kIdbStore].name).autoIncrement;
  }
  get indexNames() {
    return nameList(storeData(this[kIdbStore].transaction, this[kIdbStore].name).indexes.keys());
  }
  get transaction() {
    return this[kIdbStore].transaction;
  }
  put(value, key) {
    return storeWrite(this, value, key, false);
  }
  add(value, key) {
    return storeWrite(this, value, key, true);
  }
  get(query) {
    const range = toKeyRange(query, true);
    return storeRead(this, (store) => {
      const record = store.records.find((record) => inRange(range, record.key));
      return record ? structuredClone(record.value) : undefined;
    });
  }
  getKey(query) {
    const range = toKeyRange(query, true);
    return storeRead(this, (store) => store.records.find((record) => inRange(range, record.key))?.key);
  }
  getAll(query, count) {
    const range = toKeyRange(query);
    return storeRead(this, (store) =>
      store.records
        .filter((record) => inRange(range, record.key))
        .slice(0, count ? count >>> 0 : undefined)
        .map((record) => structuredClone(record.value)),
    );
  }
  getAllKeys(query, count) {
    const range = toKeyRange(query);
    return storeRead(this, (store) =>
      store.records
        .filter((record) => inRange(range, record.key))
        .slice(0, count ? count >>> 0 : undefined)
        .map((record) => record.key),
    );
  }
  count(query) {
    const range = toKeyRange(query);
    return storeRead(this, (store) => store.records.filter((record) => inRange(range, record.key)).length);
  }
  delete(query) {
    const { transaction, name } = this[kIdbStore];
    const state = transaction[kIdbTransaction];
    if (state.mode === 'readonly') throw idbError('ReadOnlyError', 'the transaction is read-only');
    const range = toKeyRange(query, true);
    const store = storeData(transaction, name);
    return state.request(this, () => {
      state.touch(store);
      store.records = store.records.filter((record) => !inRange(range, record.key));
      return undefined;
    });
  }
  clear() {
    const { transaction, name } = this[kIdbStore];
    const state = transaction[kIdbTransaction];
    if (state.mode === 'readonly') throw idbError('ReadOnlyError', 'the transaction is read-only');
    const store = storeData(transaction, name);
    return state.request(this, () => {
      state.touch(store);
      store.records = [];
      return undefined;
    });
  }
  openCursor(query, direction = 'next') {
    return openCursor(this, toKeyRange(query), direction, true, null);
  }
  openKeyCursor(query, direction = 'next') {
    return openCursor(this, toKeyRange(query), direction, false, null);
  }
  index(name) {
    const { transaction } = this[kIdbStore];
    if (transaction[kIdbTransaction].finished) throw idbError('InvalidStateError', 'the transaction has finished');
    const store = storeData(transaction, this[kIdbStore].name);
    if (!store.indexes.has(String(name))) throw idbError('NotFoundError', 'no index named ' + name);
    const index = Object.create(IDBIndex.prototype);
    index[kIdbIndex] = { store: this, name: String(name) };
    return index;
  }
  createIndex(name, keyPath, options = {}) {
    const { transaction } = this[kIdbStore];
    const state = transaction[kIdbTransaction];
    if (state.mode !== 'versionchange') throw idbError('InvalidStateError', 'indexes are created in an upgrade');
    if (!state.isActive()) throw idbError('TransactionInactiveError', 'the upgrade transaction is not active');
    const store = storeData(transaction, this[kIdbStore].name);
    name = String(name);
    if (store.indexes.has(name)) throw idbError('ConstraintError', 'an index named ' + name + ' exists');
    if (!validKeyPath(keyPath)) throw idbError('SyntaxError', 'invalid key path');
    const multiEntry = Boolean(options?.multiEntry);
    if (multiEntry && Array.isArray(keyPath)) throw idbError('InvalidAccessError', 'a multi-entry index needs a single key path');
    const index = { name, keyPath, unique: Boolean(options?.unique), multiEntry };
    state.touch(store);
    store.indexes.set(name, index);
    try {
      checkUnique(store, { key: undefined, value: undefined }, false);
      if (index.unique) {
        const entries = indexEntries(store, index);
        for (let at = 1; at < entries.length; at++) {
          if (compareKeys(entries[at - 1].key, entries[at].key) === 0) {
            state.abort(idbError('ConstraintError', 'existing records break the unique index ' + name));
            break;
          }
        }
      }
    } catch (_) {}
    return this.index(name);
  }
  deleteIndex(name) {
    const { transaction } = this[kIdbStore];
    const state = transaction[kIdbTransaction];
    if (state.mode !== 'versionchange') throw idbError('InvalidStateError', 'indexes are deleted in an upgrade');
    const store = storeData(transaction, this[kIdbStore].name);
    if (!store.indexes.has(String(name))) throw idbError('NotFoundError', 'no index named ' + name);
    state.touch(store);
    store.indexes.delete(String(name));
  }
}
const kIdbStore = Symbol('idbStore');
const makeStore = (transaction, name) => {
  const store = Object.create(IDBObjectStore.prototype);
  store[kIdbStore] = { transaction, name };
  return store;
};

class IDBIndex {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
  }
  get name() {
    return this[kIdbIndex].name;
  }
  get objectStore() {
    return this[kIdbIndex].store;
  }
  get keyPath() {
    return indexData(this).index.keyPath;
  }
  get unique() {
    return indexData(this).index.unique;
  }
  get multiEntry() {
    return indexData(this).index.multiEntry;
  }
  get(query) {
    const range = toKeyRange(query, true);
    return indexRead(this, (entries) => {
      const entry = entries.find((entry) => inRange(range, entry.key));
      return entry ? structuredClone(entry.value) : undefined;
    });
  }
  getKey(query) {
    const range = toKeyRange(query, true);
    return indexRead(this, (entries) => entries.find((entry) => inRange(range, entry.key))?.primaryKey);
  }
  getAll(query, count) {
    const range = toKeyRange(query);
    return indexRead(this, (entries) =>
      entries
        .filter((entry) => inRange(range, entry.key))
        .slice(0, count ? count >>> 0 : undefined)
        .map((entry) => structuredClone(entry.value)),
    );
  }
  getAllKeys(query, count) {
    const range = toKeyRange(query);
    return indexRead(this, (entries) =>
      entries
        .filter((entry) => inRange(range, entry.key))
        .slice(0, count ? count >>> 0 : undefined)
        .map((entry) => entry.primaryKey),
    );
  }
  count(query) {
    const range = toKeyRange(query);
    return indexRead(this, (entries) => entries.filter((entry) => inRange(range, entry.key)).length);
  }
  openCursor(query, direction = 'next') {
    return openCursor(this[kIdbIndex].store, toKeyRange(query), direction, true, this);
  }
  openKeyCursor(query, direction = 'next') {
    return openCursor(this[kIdbIndex].store, toKeyRange(query), direction, false, this);
  }
}
const kIdbIndex = Symbol('idbIndex');

// Cursors (§2.10): each `continue` is another request answered on the same
// request object, from where the cursor stood.
class IDBCursor {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
  }
  get source() {
    return this[kIdbCursor].source;
  }
  get direction() {
    return this[kIdbCursor].direction;
  }
  get key() {
    return this[kIdbCursor].entry?.key;
  }
  get primaryKey() {
    return this[kIdbCursor].entry?.primaryKey;
  }
  get request() {
    return this[kIdbCursor].request;
  }
  advance(count) {
    count = Number(count);
    if (!(count >= 1)) throw new TypeError('advance needs a positive count');
    this[kIdbCursor].move(count, undefined);
  }
  continue(key) {
    this[kIdbCursor].move(1, key === undefined ? undefined : requireKey(key));
  }
  continuePrimaryKey(key, primaryKey) {
    this[kIdbCursor].move(1, requireKey(key), requireKey(primaryKey));
  }
  update(value) {
    const cursor = this[kIdbCursor];
    if (!cursor.withValue) throw idbError('InvalidStateError', 'a key cursor has no value');
    const store = cursor.store;
    const data = storeData(store[kIdbStore].transaction, store[kIdbStore].name);
    return data.keyPath === null ? store.put(value, cursor.entry.primaryKey) : store.put(value);
  }
  delete() {
    const cursor = this[kIdbCursor];
    if (!cursor.withValue) throw idbError('InvalidStateError', 'a key cursor has no value');
    return cursor.store.delete(cursor.entry.primaryKey);
  }
}
class IDBCursorWithValue extends IDBCursor {
  get value() {
    const entry = this[kIdbCursor].entry;
    return entry ? structuredClone(entry.value) : undefined;
  }
}
const kIdbCursor = Symbol('idbCursor');

const openCursor = (store, range, direction, withValue, index) => {
  if (!['next', 'nextunique', 'prev', 'prevunique'].includes(direction)) {
    throw new TypeError('invalid cursor direction ' + direction);
  }
  const transaction = store[kIdbStore].transaction;
  const state = transaction[kIdbTransaction];
  const cursor = Object.create((withValue ? IDBCursorWithValue : IDBCursor).prototype);
  const reverse = direction.startsWith('prev');
  const unique = direction.endsWith('unique');
  const entries = () => {
    const data = storeData(transaction, store[kIdbStore].name);
    const list = index
      ? indexEntries(data, data.indexes.get(index[kIdbIndex].name))
      : data.records.map((record) => ({ key: record.key, primaryKey: record.key, value: record.value }));
    return list.filter((entry) => inRange(range, entry.key));
  };
  const order = (a, b) => compareKeys(a.key, b.key) || compareKeys(a.primaryKey, b.primaryKey);
  const inner = {
    source: index || store,
    direction,
    withValue,
    store,
    entry: null,
    request: null,
    gotValue: false,
    move(count, target, targetPrimary) {
      if (!inner.gotValue) throw idbError('InvalidStateError', 'the cursor is moving or has finished');
      inner.gotValue = false;
      const request = inner.request;
      request[kIdbRequest].done = false;
      state.requeue(request, () => step(count, target, targetPrimary));
    },
  };
  cursor[kIdbCursor] = inner;
  const step = (count, target, targetPrimary) => {
    let list = entries();
    if (reverse) list = list.reverse();
    const position = inner.entry;
    let candidates = list.filter((entry) => {
      if (position) {
        const relative = order(entry, position);
        if (reverse ? relative >= 0 : relative <= 0) return false;
        if (unique && compareKeys(entry.key, position.key) === 0) return false;
      }
      if (target !== undefined) {
        const relative = compareKeys(entry.key, target);
        if (reverse ? relative > 0 : relative < 0) return false;
        if (targetPrimary !== undefined && relative === 0) {
          const primary = compareKeys(entry.primaryKey, targetPrimary);
          if (reverse ? primary > 0 : primary < 0) return false;
        }
      }
      return true;
    });
    if (unique) {
      candidates = candidates.filter((entry, at) => at === 0 || compareKeys(entry.key, candidates[at - 1].key) !== 0);
      if (reverse) {
        // A reverse unique cursor stands on the first record of each key.
        candidates = candidates.map((entry) => list.filter((other) => compareKeys(other.key, entry.key) === 0).pop());
      }
    }
    const next = candidates[count - 1];
    if (!next) {
      inner.entry = null;
      return null;
    }
    inner.entry = next;
    inner.gotValue = true;
    return cursor;
  };
  const request = state.request(index || store, () => step(1));
  inner.request = request;
  return request;
};

// §4.1 IDBFactory.
class IDBFactory {
  constructor(token) {
    if (token !== kIdbInternal) throw new TypeError('Illegal constructor');
  }
  open(name, version) {
    if (arguments.length < 1) throw new TypeError("Failed to execute 'open' on 'IDBFactory': 1 argument required");
    name = String(name);
    if (version !== undefined) {
      version = Number(version);
      if (!Number.isInteger(version) || version < 1 || version > Number.MAX_SAFE_INTEGER) {
        throw new TypeError('the version must be a positive integer');
      }
    }
    const request = new IDBOpenDBRequest(kIdbInternal);
    idbTask(() => {
      let data = idbDatabases.get(name);
      const fresh = !data;
      if (fresh) data = { version: 0, stores: new Map() };
      const wanted = version === undefined ? Math.max(data.version, 1) : version;
      const requestState = request[kIdbRequest];
      if (wanted < data.version) {
        requestState.done = true;
        requestState.error = idbError('VersionError', 'the requested version is lower than the existing one');
        idbFire(request, 'error', { bubbles: true, cancelable: true });
        return;
      }
      if (fresh) idbDatabases.set(name, data);
      const db = new IDBDatabase(kIdbInternal, name, data);
      if (!idbConnections.has(name)) idbConnections.set(name, new Set());
      if (wanted === data.version) {
        idbConnections.get(name).add(db);
        requestState.done = true;
        requestState.result = db;
        idbFire(request, 'success');
        return;
      }
      // An upgrade: other connections hear `versionchange` first.
      const oldVersion = data.version;
      for (const other of idbConnections.get(name)) {
        idbFire(other, 'versionchange', {
          event: new IDBVersionChangeEvent('versionchange', { oldVersion, newVersion: wanted }),
        });
      }
      idbConnections.get(name).add(db);
      const transaction = makeTransaction(db, [], 'versionchange', (committed) => {
        db[kIdbDatabase].upgrade = null;
        request[kIdbRequest].transaction = null;
        if (committed && !db[kIdbDatabase].closed) {
          idbFire(request, 'success');
        } else {
          idbConnections.get(name)?.delete(db);
          if (fresh && !committed) idbDatabases.delete(name);
          requestState.result = undefined;
          requestState.error = idbError('AbortError', 'the upgrade was aborted');
          idbFire(request, 'error', { bubbles: true, cancelable: true });
        }
      });
      const tstate = transaction[kIdbTransaction];
      tstate.storesBefore = new Map(data.stores);
      tstate.versionBefore = oldVersion;
      data.version = wanted;
      db[kIdbDatabase].upgrade = tstate;
      requestState.done = true;
      requestState.result = db;
      requestState.transaction = transaction;
      idbFire(request, 'upgradeneeded', {
        event: new IDBVersionChangeEvent('upgradeneeded', { oldVersion, newVersion: wanted }),
      });
    });
    return request;
  }
  deleteDatabase(name) {
    name = String(name);
    const request = new IDBOpenDBRequest(kIdbInternal);
    idbTask(() => {
      const data = idbDatabases.get(name);
      const oldVersion = data ? data.version : 0;
      for (const other of idbConnections.get(name) ?? []) {
        idbFire(other, 'versionchange', {
          event: new IDBVersionChangeEvent('versionchange', { oldVersion, newVersion: null }),
        });
      }
      idbDatabases.delete(name);
      idbConnections.delete(name);
      request[kIdbRequest].done = true;
      request[kIdbRequest].result = undefined;
      idbFire(request, 'success', {
        event: new IDBVersionChangeEvent('success', { oldVersion, newVersion: null }),
      });
    });
    return request;
  }
  databases() {
    return Promise.resolve([...idbDatabases].map(([name, data]) => ({ name, version: data.version })));
  }
  cmp(first, second) {
    return compareKeys(requireKey(first), requireKey(second));
  }
}

for (const [name, value] of Object.entries({
  IDBFactory,
  IDBDatabase,
  IDBTransaction,
  IDBObjectStore,
  IDBIndex,
  IDBCursor,
  IDBCursorWithValue,
  IDBKeyRange,
  IDBRequest,
  IDBOpenDBRequest,
  IDBVersionChangeEvent,
})) {
  Object.defineProperty(value.prototype, Symbol.toStringTag, { value: name, configurable: true });
  defineGlobal(name, value);
}
defineGlobal('indexedDB', new IDBFactory(kIdbInternal));
