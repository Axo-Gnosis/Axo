((send, emit, read, write, inFlightLimit, callLimit) => {
  const { create, defineProperties, freeze, getPrototypeOf } = Object;
  const objectPrototype = Object.prototype;
  const { parse, stringify } = JSON;
  const { isArray } = Array;
  const NativePromise = Promise, NativeError = Error, NativeTypeError = TypeError;
  const pending = new Map();
  const get = pending.get.bind(pending);
  const set = pending.set.bind(pending);
  const remove = pending.delete.bind(pending);
  let nextId = 0;
  const queue = limit => ({ limit, inFlight: 0, head: undefined, tail: undefined });
  const calls = queue(inFlightLimit), lookups = queue(1);

  // Keep undispatched work on the VM heap. A large Promise.all must neither
  // overflow the bridge nor queue its full argument set in Rust memory. A
  // lookup can still proceed when all tool call slots are occupied.
  function pump(q, draining = false) {
    while (q.head && q.inFlight < q.limit) {
      const { id, op, json, error, reject } = q.head;
      q.head = q.head.next;
      if (!q.head) q.tail = undefined;
      try {
        send(id, op, json);
        q.inFlight++;
      } catch (cause) {
        remove(id);
        if (!draining) {
          error.message = cause.message;
          reject(error);
        }
      }
    }
  }

  function request(op, payload) {
    return new NativePromise((resolve, reject) => {
      if (nextId >= callLimit) throw new NativeError(`Tool call limit reached (${callLimit})`);
      const json = stringify(payload);
      const error = new NativeError(''); // Keep the calling script's stack.
      const id = ++nextId;
      const q = op === 'call' ? calls : lookups;
      set(id, { resolve, reject, error, q });
      const entry = { id, op, json, error, reject, next: undefined };
      if (q.tail) q.tail.next = entry;
      else q.head = entry;
      q.tail = entry;
      pump(q);
    });
  }

  function print(...values) {
    let text = '';
    for (let i = 0; i < values.length; i++) {
      if (i) text += ' ';
      const value = values[i];
      text += typeof value === 'string' ? value : (stringify(value, null, 2) ?? '');
    }
    emit(text);
  }

  const tools = new Proxy(freeze(create(null)), freeze({
    get: (_, name) => typeof name === 'string' ? async (args = {}) => {
      if (args === null || typeof args !== 'object' || isArray(args)) {
        throw new NativeTypeError('Tool arguments must be a plain object');
      }
      const proto = getPrototypeOf(args);
      if (proto !== objectPrototype && proto !== null) {
        throw new NativeTypeError('Tool arguments must be a plain object');
      }
      return request('call', [name, args]);
    } : undefined,
  }));
  const console = freeze({ log: print, info: print, warn: print, error: print, debug: print });
  const keyOf = key => {
    if (typeof key !== 'string') throw new NativeTypeError('Store key must be a string');
    return key;
  };
  const store = (key, value) => {
    keyOf(key);
    const json = value === undefined ? undefined : stringify(value);
    if (value !== undefined && json === undefined) throw new NativeTypeError('Value is not serializable');
    write(key, json);
  };
  const load = key => {
    const json = read(keyOf(key));
    return json === undefined ? undefined : parse(json);
  };
  const searchTools = async query => {
    if (typeof query !== 'string') throw new NativeTypeError('Query must be a string');
    return request('search', query);
  };
  const describeTool = async name => {
    if (typeof name !== 'string') throw new NativeTypeError('Tool name must be a string');
    return request('describe', name);
  };

  for (const object of [objectPrototype, Array.prototype, Function.prototype, Promise.prototype, JSON]) freeze(object);
  defineProperties(globalThis, {
    tools: { value: tools }, print: { value: freeze(print) }, console: { value: console },
    store: { value: freeze(store) }, load: { value: freeze(load) },
    searchTools: { value: freeze(searchTools) }, describeTool: { value: freeze(describeTool) },
  });

  function finish(id) {
    const entry = get(id);
    remove(id);
    entry.q.inFlight--;
    return entry;
  }

  function settle(id, ok, payload) {
    const { resolve, reject, error, q } = finish(id);
    if (ok) {
      try { payload = parse(payload); }
      catch { ok = false; payload = 'Result too large'; }
    }
    if (ok) resolve(payload);
    else {
      error.message = payload ?? 'Result too large';
      reject(error);
    }
    pump(q);
  }
  // After the script returns, release slots and dispatch queued requests without
  // resolving promises or executing user continuations.
  function drain(id) {
    if (id != null) pump(finish(id).q, true);
    return calls.inFlight !== 0 || lookups.inFlight !== 0;
  }
  return [settle, print, drain];
})
