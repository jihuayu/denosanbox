(function installWorkerRuntime(ops, config) {
  'use strict';
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
  const slice = Array.prototype.slice;
  const thenMethod = Promise.prototype.then;
  const resolvePromise = Promise.resolve.bind(Promise);
  const makePromise = Promise;
  const objectPrototype = Object.prototype;
  const charCodeAt = String.prototype.charCodeAt;
  const callWorker = (operation, input) => ops.op_sandbox_worker(operation, input);
  const then = (promise, ok, error) => apply(thenMethod, promise, [ok, error]);
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
      if (!['message', 'error', 'messageerror', 'exit'].includes(type)) throw unsupported('Worker event type');
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
      return { __workerMessages: outbox, closed: childClosed };
    } finally { dispatching = false; outbox = []; outboxBytes = 0; }
  }

  class Worker {
    #state;
    constructor(specifier, options = {}) {
      if (!options || typeof options !== 'object') throw new TypeError('Worker options must be an object');
      const fields = keys(options);
      for (let i = 0; i < fields.length; i++) if (fields[i] !== 'type' && fields[i] !== 'name') throw unsupported('Worker option ' + fields[i]);
      if (options.type !== 'module') throw unsupported('Only explicit module Workers are supported');
      if (options.name !== undefined && (typeof options.name !== 'string' || options.name.length > 128)) throw new TypeError('Invalid Worker name');
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
      then(state.ready, undefined, error => { state.closed = true; state.release(); this.#error(error); });
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
      const safe = error instanceof Error ? error : failure('WorkerError', String(error));
      this.#emit('error', { error: safe, message: String(safe.message) });
    }
    get onmessage() { return this.#state.onmessage; }
    set onmessage(value) { this.#state.onmessage = value; }
    get onerror() { return this.#state.onerror; }
    set onerror(value) { this.#state.onerror = value; }
    addEventListener(type, callback) { this.#state.events.add(String(type), callback); }
    removeEventListener(type, callback) { this.#state.events.remove(String(type), callback); }
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
      for (let i = records.length - 1; i >= 0; i--) if (records[i].type === type && records[i].callback === callback) {
        this.removeEventListener(type, records[i].wrapper); records.splice(i, 1);
      }
      return this;
    }
    removeListener(type, callback) { return this.off(type, callback); }
    terminate() { return then(super.terminate(), () => 0); }
  }
  const parentPort = config.isChild ? freeze({
    postMessage: childPost,
    on(type, callback) { nodeChildEvents.add(String(type), callback); return parentPort; },
    once(type, callback) {
      const once = value => { nodeChildEvents.remove(String(type), once); return apply(callback, parentPort, [value]); };
      nodeChildEvents.add(String(type), once); return parentPort;
    },
    off(type, callback) { nodeChildEvents.remove(String(type), callback); return parentPort; },
    removeListener(type, callback) { nodeChildEvents.remove(String(type), callback); return parentPort; },
    close: closeChild,
  }) : null;
  const readonly = (name, value) => define(globalThis, name, { value, writable: false, configurable: false });
  if (typeof globalThis.self === 'undefined') readonly('self', globalThis);
  define(globalThis, 'onmessage', { get: () => childOnMessage, set: value => { childOnMessage = value; }, configurable: false });
  readonly('addEventListener', (type, callback) => childEvents.add(String(type), callback));
  readonly('removeEventListener', (type, callback) => childEvents.remove(String(type), callback));
  readonly('postMessage', childPost);
  readonly('close', closeChild);
  readonly('__denosanboxDispatchWorkerMessage', dispatchChild);
  readonly('__denosanboxNodeWorker', freeze(NodeWorker));
  readonly('__denosanboxParentPort', parentPort);
  readonly('__denosanboxIsMainThread', !config.isChild);
  freeze(Worker.prototype);
  freeze(NodeWorker.prototype);
  readonly('Worker', freeze(Worker));
})
