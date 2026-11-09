'use strict';
if (process.platform !== 'linux' || process.arch !== 'x64' || process.versions.node !== '24.22.1') {
  throw new Error('unsupported_platform: the experimental native gate currently covers only Node 24.22.1 on Linux x86_64');
}
const native = require('./denosanbox.node');
const prepared = new WeakMap();
class SandboxError extends Error {
  constructor(error) {
    super(error.message);
    this.name = 'SandboxError';
    this.kind = /^([a-z_]+):/.exec(error.message)?.[1] ?? 'native';
  }
}
function sync(fn) { try { return fn(); } catch (error) { throw new SandboxError(error); } }
async function asyncCall(fn) { try { return await fn(); } catch (error) { throw new SandboxError(error); } }
class PreparedPlugin {
  constructor(source, entrypoint) { prepared.set(this, {source, entrypoint}); Object.freeze(this); }
}
class Engine {
  #native;
  #controllers = new Map();
  constructor(options = {}) { this.#native = sync(() => new native.NativeEngine(options.hostCapabilities ?? [])); }
  registerHost(name, callback, options = {}) {
    if (typeof callback !== 'function') throw new TypeError('host callback must be a function');
    const controllers = this.#controllers;
    const invoke = async (contextJson, inputJson) => {
      const metadata = JSON.parse(contextJson);
      const controller = new AbortController();
      let active = controllers.get(metadata.sandboxId);
      if (!active) { active = new Set(); controllers.set(metadata.sandboxId, active); }
      active.add(controller);
      const timer = setTimeout(() => controller.abort(), (metadata.remainingMs ?? 0) + 1);
      const context = Object.freeze({...metadata, signal: controller.signal});
      try {
        const output = JSON.stringify(await callback(context, JSON.parse(inputJson)));
        if (output === undefined) throw new TypeError('host callback must return a JSON value');
        return output;
      } finally {
        clearTimeout(timer);
        active.delete(controller);
        if (active.size === 0) controllers.delete(metadata.sandboxId);
      }
    };
    sync(() => this.#native.registerHost(name, invoke, options.inputLimit ?? 1048576, options.outputLimit ?? 1048576));
  }
  prepare(source, entrypoint = 'main.js') {
    if (typeof source !== 'string' || typeof entrypoint !== 'string') throw new TypeError('source and entrypoint must be strings');
    sync(() => this.#native.prepare(source, entrypoint));
    return new PreparedPlugin(source, entrypoint);
  }
  async create(plugin, options = {}) {
    const value = prepared.get(plugin);
    if (!value) throw new TypeError('expected a PreparedPlugin returned by Engine.prepare');
    const handle = await asyncCall(() => this.#native.create(value.source, value.entrypoint, JSON.stringify(options.hostGrants ?? {})));
    return new Sandbox(handle, this.#controllers);
  }
  shutdown() {
    for (const active of this.#controllers.values()) for (const controller of active) controller.abort();
    return asyncCall(() => this.#native.shutdown());
  }
  async [Symbol.asyncDispose]() { if (!await this.shutdown()) throw new SandboxError(new Error("incomplete_close: host work still owns resources")); }
}
class Sandbox {
  #native;
  #controllers;
  constructor(handle, controllers) { this.#native = handle; this.#controllers = controllers; }
  call(exportName, input, options = {}) {
    const json = JSON.stringify(input);
    if (json === undefined) return Promise.reject(new TypeError('input must be JSON serializable'));
    const signal = options.signal;
    const abort = () => this.cancel();
    if (signal?.aborted) { abort(); return Promise.reject(signal.reason ?? new Error('invocation cancelled')); }
    signal?.addEventListener('abort', abort, {once: true});
    return asyncCall(async () => JSON.parse(await this.#native.call(exportName, json, options.timeoutMs)))
      .finally(() => signal?.removeEventListener('abort', abort));
  }
  #abortCallbacks() {
    const active = this.#controllers.get(this.#native.id);
    if (active) for (const controller of active) controller.abort();
  }
  cancel() { this.#abortCallbacks(); this.#native.cancel(); }
  close() { this.#abortCallbacks(); return asyncCall(() => this.#native.close()); }
  async [Symbol.asyncDispose]() { if (!await this.close()) throw new SandboxError(new Error("incomplete_close: host work still owns resources")); }
}
module.exports = {Engine, PreparedPlugin, Sandbox, SandboxError};
