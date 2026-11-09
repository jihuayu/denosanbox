// Trusted bootstrap. A single IIFE: trusted ops are captured as closure values
// first, the raw `Deno` namespace is hidden, and only then are mediated globals
// installed. No lexical or global helper names survive (nothing like
// `__denosanboxBootstrap`/`__denosanboxConfig` is left for guests to call).
(function () {
  'use strict';
  const rawCore = typeof Deno === 'object' && Deno !== null ? Deno.core : undefined;
  const ops = rawCore && rawCore.ops;
  if (!ops) throw new Error('trusted ops unavailable');
  const config = __SANDBOX_CONFIG__;
  const define = Object.defineProperty;
  const freeze = Object.freeze;
  const keys = Object.keys;
  const getPrototypeOf = Object.getPrototypeOf;
  const isArray = Array.isArray;
  const isView = ArrayBuffer.isView;
  const ArrayBufferType = ArrayBuffer;
  const stringify = JSON.stringify;
  const parse = JSON.parse;
  const apply = Reflect.apply;
  const deleteProperty = Reflect.deleteProperty;
  const slice = Array.prototype.slice;
  const thenMethod = Promise.prototype.then;
  const resolvePromise = Promise.resolve.bind(Promise);
  const objectPrototype = Object.prototype;
  const charCodeAt = String.prototype.charCodeAt;
  const stringifyText = String;
  const opWorker = ops.op_sandbox_worker;
  const opLog = ops.op_sandbox_log;
  const opFetch = ops.op_sandbox_fetch;
  const opResource = ops.op_sandbox_resource;
  const opResourceAsync = ops.op_sandbox_resource_async;
  const opHost = ops.op_sandbox_host;
  const callWorker = (operation, input) => apply(opWorker, undefined, [operation, stringify(input)]);
  const then = (promise, ok, error) => apply(thenMethod, promise, [ok, error]);
  const encode = (value) => {
    const text = stringify(value);
    if (typeof text !== 'string' || text.length > config.maxMessageBytes) {
      throw new RangeError('message limit exceeded');
    }
    return text;
  };
  let activeWorkers = 0;
  let dispatching = false;
  let childClosed = false;
  let outbox = [];
  let outboxBytes = 0;

  function failure(name, message) {
    const error = new Error(message);
    error.name = name;
    return error;
  }
  function unsupported(feature) { return failure('UnsupportedFeature', feature); }
  function utf8Bytes(text) {
    let bytes = 0;
    for (let i = 0; i < text.length; i++) {
      const unit = apply(charCodeAt, text, [i]);
      if (unit < 0x80) bytes++;
      else if (unit < 0x800) bytes += 2;
      else if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length) {
        const next = apply(charCodeAt, text, [i + 1]);
        if (next >= 0xdc00 && next <= 0xdfff) { bytes += 4; i++; }
        else bytes += 3;
      } else bytes += 3;
    }
    return bytes;
  }
  function cloneMessage(value, transfer = []) {
    if (!isArray(transfer) || transfer.length) throw failure('DataCloneError', 'Transfer lists are unsupported');
    let encoded;
    try {
      encoded = stringify(value, (_key, item) => {
        const kind = typeof item;
        if (kind === 'undefined' || kind === 'function' || kind === 'symbol' || kind === 'bigint') {
          throw failure('DataCloneError', 'Only JSON values can cross Worker ports');
        }
        if (item && kind === 'object') {
          const prototype = getPrototypeOf(item);
          if (isView(item) || item instanceof ArrayBufferType || (!isArray(item) && prototype !== objectPrototype && prototype !== null)) {
            throw failure('DataCloneError', 'Only JSON objects and arrays can cross Worker ports');
          }
        }
        return item;
      });
    } catch (error) {
      if (error && error.name === 'DataCloneError') throw error;
      throw failure('DataCloneError', 'Worker message is not JSON serializable');
    }
    const bytes = utf8Bytes(encoded);
    if (bytes > config.maxMessageBytes) throw failure('QuotaExceededError', 'Worker message byte limit exceeded');
    return { value: parse(encoded), bytes };
  }
  function event(type, target, extra) {
    return freeze({ type, target, currentTarget: target, ...extra });
  }
  class Events {
    constructor() { this.listeners = Object.create(null); }
    add(type, callback) {
      if (type !== 'message' && type !== 'error' && type !== 'messageerror' && type !== 'exit') {
        throw unsupported('Worker event type');
      }
      if (typeof callback !== 'function') throw new TypeError('Worker listener must be a function');
      const list = this.listeners[type] || (this.listeners[type] = []);
      if (list.indexOf(callback) >= 0) return;
      if (list.length >= config.maxQueuedMessages) throw failure('QuotaExceededError', 'Too many Worker listeners');
      list.push(callback);
    }
    remove(type, callback) {
      const list = this.listeners[type];
      if (!list) return;
      for (let i = list.length - 1; i >= 0; i--) if (list[i] === callback) list.splice(i, 1);
    }
    snapshot(type) { return apply(slice, this.listeners[type] || [], []); }
  }
  const childEvents = new Events();
  const nodeChildEvents = new Events();
  let childOnMessage = null;
  function childPost(value, transfer) {
    if (!config.isChild) throw unsupported('The root sandbox has no parent message port');
    if (!dispatching || childClosed) throw unsupported('Child messages require an active message handler');
    const message = cloneMessage(value, transfer);
    if (outbox.length >= config.maxQueuedMessages || outboxBytes + message.bytes > config.maxQueuedBytes) {
      throw failure('QuotaExceededError', 'Child outgoing message queue is full');
    }
    outbox.push(message.value);
    outboxBytes += message.bytes;
  }
  function closeChild() {
    if (!config.isChild || !dispatching) throw unsupported('Child close requires an active message handler');
    childClosed = true;
  }
  async function dispatchChild(value) {
    if (!config.isChild) throw unsupported('A root sandbox cannot receive parent messages');
    if (dispatching) throw unsupported('Synchronous Worker re-entry');
    if (childClosed) return { __workerMessages: [], closed: true };
    dispatching = true;
    outbox = [];
    outboxBytes = 0;
    try {
      const message = event('message', globalThis, { data: value });
      if (typeof childOnMessage === 'function') await apply(childOnMessage, globalThis, [message]);
      const web = childEvents.snapshot('message');
      for (let i = 0; i < web.length; i++) await apply(web[i], globalThis, [message]);
      const node = nodeChildEvents.snapshot('message');
      for (let i = 0; i < node.length; i++) await apply(node[i], parentPort, [value]);
      return { __workerMessages: outbox.slice(0), closed: childClosed };
    } finally { dispatching = false; outbox = []; outboxBytes = 0; }
  }

  class Worker {
    #state;
    constructor(specifier, options = {}) {
      if (!options || typeof options !== 'object') throw new TypeError('Worker options must be an object');
      const fields = keys(options);
      for (let i = 0; i < fields.length; i++) {
        if (fields[i] !== 'type' && fields[i] !== 'name') throw unsupported('Worker option ' + fields[i]);
      }
      if (options.type !== 'module') throw unsupported('Only explicit module Workers are supported');
      if (options.name !== undefined && (typeof options.name !== 'string' || options.name.length > 128)) {
        throw new TypeError('Invalid Worker name');
      }
      if (typeof specifier !== 'string' && specifier && typeof specifier.href === 'string') specifier = specifier.href;
      if (typeof specifier !== 'string' || !specifier.length || specifier.length > 4096) throw new TypeError('Invalid Worker entrypoint');
      if (activeWorkers >= config.maxChildren) throw failure('QuotaExceededError', 'Worker child admission is full');
      activeWorkers++;
      const state = { closed: false, released: false, count: 0, bytes: 0, events: new Events(), onmessage: null, onerror: null, terminating: null };
      this.#state = state;
      state.release = () => { if (!state.released) { state.released = true; activeWorkers--; } };
      const request = { type: 'module' };
      if (options.name !== undefined) request.name = options.name;
      state.ready = callWorker('spawn', { specifier, options: request });
      then(state.ready, undefined, error => {
        state.closed = true;
        state.release();
        this.#error(error);
      });
    }
    #emit(type, extra) {
      const state = this.#state;
      const item = event(type, this, extra);
      const handler = type === 'message' ? state.onmessage : type === 'error' ? state.onerror : null;
      if (typeof handler === 'function') { try { apply(handler, this, [item]); } catch (_) {} }
      const list = state.events.snapshot(type);
      for (let i = 0; i < list.length; i++) { try { apply(list[i], this, [item]); } catch (_) {} }
    }
    #error(error) {
      const safe = error instanceof Error ? error : failure('WorkerError', stringifyText(error));
      this.#emit('error', { error: safe, message: stringifyText(safe.message) });
    }
    get onmessage() { return this.#state.onmessage; }
    set onmessage(value) { this.#state.onmessage = value; }
    get onerror() { return this.#state.onerror; }
    set onerror(value) { this.#state.onerror = value; }
    addEventListener(type, callback) { this.#state.events.add(stringifyText(type), callback); }
    removeEventListener(type, callback) { this.#state.events.remove(stringifyText(type), callback); }
    postMessage(value, transfer) {
      const state = this.#state;
      if (state.closed) throw failure('InvalidStateError', 'Worker is closed');
      const copy = cloneMessage(value, transfer);
      if (state.count >= config.maxQueuedMessages || state.bytes + copy.bytes > config.maxQueuedBytes) {
        throw failure('QuotaExceededError', 'Worker pending message queue is full');
      }
      state.count++;
      state.bytes += copy.bytes;
      const finish = () => { state.count--; state.bytes -= copy.bytes; };
      const submitted = then(state.ready, id => state.closed ? null : callWorker('post', { id, message: copy.value }));
      then(submitted, reply => {
        try {
          if (state.closed || reply === null) return;
          if (reply && isArray(reply.__workerMessages)) {
            if (reply.__workerMessages.length > config.maxQueuedMessages) throw failure('QuotaExceededError', 'Worker reply batch is full');
            for (let i = 0; i < reply.__workerMessages.length; i++) this.#emit('message', { data: reply.__workerMessages[i] });
            if (reply.closed) { state.closed = true; state.release(); this.#emit('exit', { code: 0 }); }
          } else this.#emit('message', { data: reply });
        } catch (error) { this.#error(error); }
        finally { finish(); }
      }, error => { finish(); if (!state.closed) this.#error(error); });
    }
    // This profile intentionally returns a joinable Promise from terminate.
    terminate() {
      const state = this.#state;
      if (state.terminating) return state.terminating;
      if (state.closed) { state.release(); return resolvePromise(); }
      state.closed = true;
      state.terminating = then(state.ready,
        id => then(callWorker('terminate', { id }), () => { state.release(); this.#emit('exit', { code: 0 }); }),
        error => { state.release(); throw error; });
      return state.terminating;
    }
  }

  const nodeWrappers = new WeakMap();
  class NodeWorker extends Worker {
    constructor(specifier, options = {}) { super(specifier, { ...options, type: options.type ?? 'module' }); }
    on(type, callback) {
      if (typeof callback !== 'function') throw new TypeError('Worker listener must be a function');
      const wrapper = item => apply(callback, this, [type === 'message' ? item.data : type === 'error' ? item.error : item.code]);
      let records = nodeWrappers.get(this);
      if (!records) { records = []; nodeWrappers.set(this, records); }
      records.push({ type, callback, wrapper });
      this.addEventListener(type, wrapper);
      return this;
    }
    once(type, callback) {
      const once = value => { this.off(type, once); return apply(callback, this, [value]); };
      return this.on(type, once);
    }
    off(type, callback) {
      const records = nodeWrappers.get(this) || [];
      for (let i = records.length - 1; i >= 0; i--) {
        if (records[i].type === type && records[i].callback === callback) {
          this.removeEventListener(type, records[i].wrapper);
          records.splice(i, 1);
        }
      }
      return this;
    }
    removeListener(type, callback) { return this.off(type, callback); }
    terminate() { return then(super.terminate(), () => 0); }
  }

  const parentPort = config.isChild ? freeze({
    postMessage: childPost,
    on(type, callback) { nodeChildEvents.add(stringifyText(type), callback); return parentPort; },
    once(type, callback) {
      const once = value => { nodeChildEvents.remove(stringifyText(type), once); return apply(callback, parentPort, [value]); };
      nodeChildEvents.add(stringifyText(type), once);
      return parentPort;
    },
    off(type, callback) { nodeChildEvents.remove(stringifyText(type), callback); return parentPort; },
    removeListener(type, callback) { nodeChildEvents.remove(stringifyText(type), callback); return parentPort; },
    close: closeChild,
  }) : null;

  // Mediated resources (all frozen; their ops live only in closures above).
  const resource = (name, ...args) => apply(opResource, undefined, [name, encode(args)]);
  const sandbox = freeze({
    fs: freeze({
      readTextFile: (path) => apply(opResourceAsync, undefined, ['read', encode([path])]),
      writeTextFile: (path, data) => apply(opResourceAsync, undefined, ['write', encode([path, data])]),
    }),
    env: freeze({
      get: (name) => resource('env_get', name),
      set: (name, value) => resource('env_set', name, value),
    }),
    host: freeze({ protocolVersion: 1, call: (name, input) => apply(opHost, undefined, [name, encode(input)]) }),
    cwd: () => resource('cwd'),
    chdir: (path) => apply(opResourceAsync, undefined, ['chdir', encode([path])]),
    exit: () => resource('exit'),
  });
  const fetchImpl = async (url, init = {}) => {
    if (typeof url !== 'string') throw new TypeError('fetch supports a URL string');
    const text = init.body === undefined ? '' : stringifyText(init.body);
    if (text.length > config.maxMessageBytes) throw new RangeError('request body exceeds limit');
    const input = stringify({ url, method: init.method || 'GET', headers: init.headers || {}, body: text });
    if (input.length > config.maxMessageBytes) throw new RangeError('request exceeds limit');
    const response = await apply(opFetch, undefined, [input]);
    const bytes = Uint8Array.from(response.body);
    const decode = () => decodeURIComponent(escape(Array.from(bytes, c => String.fromCharCode(c)).join('')));
    return freeze({
      status: response.status,
      ok: response.status >= 200 && response.status < 300,
      url: response.url,
      headers: freeze({ get: (name) => response.headers[stringifyText(name).toLowerCase()] ?? null }),
      text: async () => decode(),
      json: async () => parse(decode()),
      arrayBuffer: async () => bytes.buffer,
    });
  };
  // The native op enforces the effective per-invocation log budget; this JS
  // check keeps a single record bounded before serialization.
  const emitLog = (...args) => {
    const text = args.map(arg => stringifyText(arg)).join(' ');
    if (utf8Bytes(text) > config.maxLogRecordBytes) throw new RangeError('log record limit exceeded');
    apply(opLog, undefined, [text]);
  };
  const consoleApi = freeze({ log: emitLog, info: emitLog, warn: emitLog, error: emitLog, debug: emitLog });

  const readonly = (name, value) => define(globalThis, name, { value, writable: false, configurable: false });
  if (typeof globalThis.self === 'undefined') readonly('self', globalThis);
  define(globalThis, 'onmessage', { get: () => childOnMessage, set: value => { childOnMessage = value; }, configurable: false });
  readonly('addEventListener', (type, callback) => childEvents.add(stringifyText(type), callback));
  readonly('removeEventListener', (type, callback) => childEvents.remove(stringifyText(type), callback));
  readonly('postMessage', childPost);
  readonly('close', closeChild);
  readonly('__denosanboxDispatchWorkerMessage', dispatchChild);
  // The `worker_threads` alias objects exist only under the host-approved
  // profile (chosen by trusted native code, never by guest input). In every
  // other profile the reserved builtin is absent from the graph, so the names
  // are unbound and `node:worker_threads` cannot be imported.
  if (config.nodeWorkerThreads) {
    readonly('__denosanboxNodeWorker', freeze(NodeWorker));
    readonly('__denosanboxParentPort', parentPort);
    readonly('__denosanboxIsMainThread', !config.isChild);
  }
  readonly('Worker', freeze(Worker));
  readonly('sandbox', sandbox);
  readonly('fetch', fetchImpl);
  readonly('console', consoleApi);
  freeze(Worker.prototype);
  freeze(NodeWorker.prototype);

  // V8 allocates `SharedArrayBuffer` backing stores outside any JS heap limit,
  // so the constructor is exiled from the heap *before* the bootstrap decides
  // whether it can be deleted: each literal creates a bounded, collectable
  // exile; allocation fails once the heap (or budgeted ArrayBuffer allocator)
  // is exhausted, and the last one is removed by the authoritative check.
  let sabExiles = 0;
  while (typeof globalThis.SharedArrayBuffer === 'function'
      && (sabExiles < 16 || globalThis.SharedArrayBuffer.prototype?.buffer?.byteLength === 0)) {
    sabExiles++;
    new globalThis.SharedArrayBuffer(0);
  }

  // Raw trusted bindings are hidden BEFORE any guest module loads. A property
  // that can be redefined or deleted is removed; otherwise it is inerted with
  // an `undefined` value when redefinition is permitted. The bootstrap never
  // freezes `globalThis` (guests may keep independent global state), so these
  // writes always take effect.
  const hideRaw = (name) => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
    if (descriptor === undefined) return;
    if (descriptor.configurable) {
      if (!deleteProperty(globalThis, name)) throw new Error('raw namespace removal failed: ' + name);
      return;
    }
    if (descriptor.writable) {
      Reflect.set(globalThis, name, undefined);
      return;
    }
    throw new Error('raw namespace is not removable: ' + name);
  };
  for (const name of ['Deno', 'coreprint', 'node', 'process', 'require']) hideRaw(name);
  // Shared-memory / native engines are neutralized, never claimed as features.
  for (const name of ['SharedArrayBuffer', 'Atomics', 'WebAssembly']) {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
    if (descriptor === undefined) continue;
    if (descriptor.configurable) {
      deleteProperty(globalThis, name);
      continue;
    }
    // `SharedArrayBuffer` reaches this branch only because its constructors
    // were exiled above; a non-configurable, non-writable binding that the
    // exile could not neutralize is left for the authoritative check below.
    if (descriptor.writable) Reflect.set(globalThis, name, undefined);
  }

  // Authoritative check: any reachable raw entry point aborts startup with a
  // typed error instead of leaving a partially trusted global behind.
  const leaked = [];
  for (const name of ['Deno', 'coreprint', 'node', 'process', 'require']) {
    if (typeof globalThis[name] !== 'undefined') leaked.push(name);
  }
  for (const name of ['SharedArrayBuffer', 'Atomics', 'WebAssembly']) {
    const kind = typeof globalThis[name];
    if (kind !== 'undefined' && kind !== 'number') leaked.push(name);
  }
  if (leaked.length) throw new Error('trusted namespace removal failed: ' + leaked.join(','));
})();
