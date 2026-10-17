// ---------------------------------------------------------------- Structured clone, as text
// The serialization a worker's messages cross threads in (HTML §2.7.3,
// StructuredSerialize / StructuredDeserialize), written as JSON so that it is
// plain text on the way. Shared by the page's glue and every worker's.
//
// Each value is a record: ['u'] undefined, ['z'] null, ['b', bool],
// ['n', number-as-string], ['s', string], ['i', bigint], or an object with a
// number that later records refer back to with ['r', number] — which is what
// keeps shared references and cycles. Functions, symbols and DOM nodes are
// not serializable, and throw DataCloneError as a browser does.

const cloneError = (what) =>
  typeof DOMException === 'function'
    ? new DOMException(`${what} could not be cloned.`, 'DataCloneError')
    : new TypeError(`${what} could not be cloned.`);

const TYPED_ARRAYS = [
  'Int8Array', 'Uint8Array', 'Uint8ClampedArray', 'Int16Array', 'Uint16Array', 'Int32Array',
  'Uint32Array', 'Float32Array', 'Float64Array', 'BigInt64Array', 'BigUint64Array',
];

const bytesToText = (bytes) => {
  let text = '';
  for (let at = 0; at < bytes.length; at += 8192) {
    text += String.fromCharCode.apply(null, bytes.subarray(at, at + 8192));
  }
  return text;
};

const textToBytes = (text) => {
  const bytes = new Uint8Array(text.length);
  for (let at = 0; at < text.length; at++) bytes[at] = text.charCodeAt(at);
  return bytes;
};

const cloneSerialize = (root) => {
  const seen = new Map();
  const encode = (value) => {
    switch (typeof value) {
      case 'undefined': return ['u'];
      case 'boolean': return ['b', value];
      case 'number': return ['n', String(value)];
      case 'string': return ['s', value];
      case 'bigint': return ['i', String(value)];
      case 'symbol': throw cloneError('Symbol()');
      case 'function': throw cloneError(String(value).slice(0, 60));
    }
    if (value === null) return ['z'];
    if (seen.has(value)) return ['r', seen.get(value)];
    const id = seen.size;
    seen.set(value, id);
    if (typeof Node === 'function' && value instanceof Node) throw cloneError(String(value));
    if (value instanceof Date) return ['d', id, value.getTime()];
    if (value instanceof RegExp) return ['x', id, value.source, value.flags];
    if (value instanceof ArrayBuffer) return ['B', id, bytesToText(new Uint8Array(value))];
    if (ArrayBuffer.isView(value)) {
      const kind = value instanceof DataView ? 'DataView' : value.constructor.name;
      const length = value instanceof DataView ? value.byteLength : value.length;
      return ['T', id, kind, encode(value.buffer), value.byteOffset, length];
    }
    if (value instanceof Map) return ['m', id, [...value].map(([k, v]) => [encode(k), encode(v)])];
    if (value instanceof Set) return ['e', id, [...value].map(encode)];
    if (value instanceof Error) return ['E', id, value.name, value.message, String(value.stack ?? '')];
    if (value instanceof Boolean || value instanceof Number || value instanceof String) {
      return ['P', id, encode(value.valueOf())];
    }
    if (typeof Blob === 'function' && value instanceof Blob) throw cloneError('Blob');
    if (Array.isArray(value)) {
      const entries = [];
      for (const key of Object.keys(value)) entries.push([key, encode(value[key])]);
      return ['a', id, value.length, entries];
    }
    const entries = [];
    for (const key of Object.keys(value)) entries.push([key, encode(value[key])]);
    return ['o', id, entries];
  };
  return JSON.stringify(encode(root));
};

const cloneDeserialize = (text) => {
  const made = [];
  const decode = (record) => {
    switch (record[0]) {
      case 'u': return undefined;
      case 'z': return null;
      case 'b': return record[1];
      case 'n': return Number(record[1]);
      case 's': return record[1];
      case 'i': return BigInt(record[1]);
      case 'r': return made[record[1]];
      case 'd': return (made[record[1]] = new Date(record[2]));
      case 'x': return (made[record[1]] = new RegExp(record[2], record[3]));
      case 'B': return (made[record[1]] = textToBytes(record[2]).buffer);
      case 'T': {
        const id = record[1];
        const buffer = decode(record[3]);
        const Kind = record[2] === 'DataView' ? DataView : globalThis[TYPED_ARRAYS.includes(record[2]) ? record[2] : 'Uint8Array'];
        return (made[id] = new Kind(buffer, record[4], record[5]));
      }
      case 'm': {
        const map = (made[record[1]] = new Map());
        for (const [k, v] of record[2]) map.set(decode(k), decode(v));
        return map;
      }
      case 'e': {
        const set = (made[record[1]] = new Set());
        for (const v of record[2]) set.add(decode(v));
        return set;
      }
      case 'E': {
        const Kind = globalThis[record[2]] && globalThis[record[2]].prototype instanceof Error ? globalThis[record[2]] : Error;
        const error = (made[record[1]] = new Kind(record[3]));
        if (record[4]) error.stack = record[4];
        return error;
      }
      case 'P': return (made[record[1]] = Object(decode(record[2])));
      case 'a': {
        const array = (made[record[1]] = new Array(record[2]));
        for (const [key, value] of record[3]) array[key] = decode(value);
        return array;
      }
      case 'o': {
        const object = (made[record[1]] = {});
        for (const [key, value] of record[2]) object[key] = decode(value);
        return object;
      }
      default: return undefined;
    }
  };
  return decode(JSON.parse(text));
};
