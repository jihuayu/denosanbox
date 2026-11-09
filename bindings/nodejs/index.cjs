'use strict';
// Exact native loading gate. Only the combination actually validated is
// accepted; everything else fails closed before the addon is loaded.
if (process.platform !== 'linux' || process.arch !== 'x64' || process.versions.node !== '24.22.1') {
  throw new Error('unsupported_platform: the experimental native gate currently covers only Node 24.22.1 on Linux x86_64');
}
const native = require('./denosanbox.node');
const prepared = new WeakMap();
const PROFILES = new Set(['javascript-only', 'pure-js-npm']);
const ACCESS = new Set(['read', 'read-write']);

class SandboxError extends Error {
  constructor(error) {
    super(error.message);
    this.name = 'SandboxError';
    this.kind = /^([a-z_]+):/.exec(error.message)?.[1] ?? 'native';
  }
}
function sync(fn) { try { return fn(); } catch (error) { throw new SandboxError(error); } }
async function asyncCall(fn) { try { return await fn(); } catch (error) { throw new SandboxError(error); } }
function jsonObject(value, label) {
  let text;
  try { text = JSON.parse(JSON.stringify(value ?? {})); } catch { throw new TypeError(`${label} must be JSON serializable`); }
  if (text === null || typeof text !== 'object' || Array.isArray(text)) throw new TypeError(`${label} must be an object`);
  return text;
}
function engineOptions(options) {
  if (options == null) return undefined;
  if (typeof options !== 'object') throw new TypeError('options must be an object');
  return {
    hostCapabilities: options.hostCapabilities ? [...options.hostCapabilities] : undefined,
    readRoots: options.readRoots ? [...options.readRoots] : undefined,
    writeRoots: options.writeRoots ? [...options.writeRoots] : undefined,
    heapTargetBytes: options.heapTargetBytes ?? undefined,
    memoryBudgetBytes: options.memoryBudgetBytes ?? undefined,
    maxSandboxes: options.maxSandboxes ?? undefined,
    maxMessageBytes: options.maxMessageBytes ?? undefined,
    deadlineMs: options.deadlineMs ?? undefined,
    queueDepth: options.queueDepth ?? undefined,
    maxLogBytes: options.maxLogBytes ?? undefined,
    // Engine hard limits (facade `EngineLimits`), enforced on every create.
    maxActiveSandboxes: options.maxActiveSandboxes ?? undefined,
    maxPendingInvocations: options.maxPendingInvocations ?? undefined,
    engineMessageBytes: options.engineMessageBytes ?? undefined,
    defaultInvocationTimeoutMs: options.defaultInvocationTimeoutMs ?? undefined,
    maxInvocationTimeoutMs: options.maxInvocationTimeoutMs ?? undefined,
    maxModules: options.maxModules ?? undefined,
    maxModuleBytes: options.maxModuleBytes ?? undefined,
    maxGraphBytes: options.maxGraphBytes ?? undefined,
  };
}
function sandboxOptions(options) {
  if (options == null) return undefined;
  if (typeof options !== 'object') throw new TypeError('options must be an object');
  return {
    defaultTimeoutMs: options.defaultTimeoutMs ?? undefined,
    memoryLimitBytes: options.memoryLimitBytes ?? undefined,
    deadlineMs: options.deadlineMs ?? undefined,
    queueDepth: options.queueDepth ?? undefined,
    maxMessageBytes: options.maxMessageBytes ?? undefined,
    maxLogBytes: options.maxLogBytes ?? undefined,
    capabilities: options.capabilities ? [...options.capabilities] : undefined,
    readRoots: options.readRoots ? [...options.readRoots] : undefined,
    writeRoots: options.writeRoots ? [...options.writeRoots] : undefined,
    scopes: undefined,
  };
}
class PreparedPlugin {
  #paths;
  #profile;
  constructor(graphJson, entrypoint, profile, paths) {
    this.#paths = Object.freeze(paths);
    this.#profile = profile;
    // The frozen graph (JSON object of module path -> source text) and its
    // validated profile travel together. `Engine.create` only ever hands this
    // JSON to the native graph API; raw source text is never re-read here.
    prepared.set(this, { graphJson, entrypoint, profile });
    Object.freeze(this);
  }
  get entrypoint() { return prepared.get(this).entrypoint; }
  get modulePaths() { return this.#paths; }
  get profile() { return this.#profile; }
}
class Engine {
  #native;
  #controllers = new Map();
  constructor(options = {}) {
    this.#native = sync(() => new native.NativeEngine(engineOptions(options)));
  }
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
    // The native single-source prepare validates the graph and path metadata;
    // the stored private value is still a unified graph JSON object.
    const paths = sync(() => this.#native.prepare(source, entrypoint));
    const graphJson = JSON.stringify({[entrypoint]: source});
    return new PreparedPlugin(graphJson, entrypoint, 'javascript-only', paths);
  }
  /**
   * Freezes a multi-module graph. `modules` maps root-relative paths
   * (.js/.mjs/.ts/.mts) to source text; static imports, dynamic imports,
   * re-exports and a guest `new Worker()` graph resolve only inside this map.
   * `profile` is explicit ('javascript-only' by default) and grants no
   * capability by itself.
   */
  prepareModules(modules, entrypoint = 'main.js', profile = 'javascript-only') {
    const graph = jsonObject(modules, 'modules');
    if (typeof entrypoint !== 'string') throw new TypeError('entrypoint must be a string');
    if (profile !== undefined && profile !== null && !PROFILES.has(profile)) {
      throw new RangeError(`invalid_configuration: unknown profile '${profile}'; expected 'javascript-only' or 'pure-js-npm'`);
    }
    const selected = profile ?? 'javascript-only';
    const modulesJson = JSON.stringify(graph);
    const paths = sync(() => this.#native.prepareModules(modulesJson, entrypoint, selected));
    return new PreparedPlugin(modulesJson, entrypoint, selected, paths);
  }
  async create(plugin, options = {}) {
    const value = prepared.get(plugin);
    if (!value) throw new TypeError('expected a PreparedPlugin returned by Engine.prepare or Engine.prepareModules');
    // Trusted scopes are captured here from the host's grant object; the guest
    // never supplies them and a forged scope in guest JSON has no effect.
    const grants = jsonObject(options.hostGrants, 'hostGrants');
    const scopes = Object.entries(grants);
    const nativeOptions = sandboxOptions(options) ?? {};
    nativeOptions.scopes = scopes;
    // `graphJson` is a frozen JSON snapshot taken at prepare time, so later
    // `create` calls cannot observe mutation and both prepare paths hand the
    // native graph API the same object-of-sources shape.
    let handle;
    try {
      handle = await asyncCall(() => this.#native.createModules(
        // Pass every argument explicitly: N-API optional-arg folding would
        // otherwise shift a trailing object into the previous slot. The
        // validated profile and graph JSON from the immutable PreparedPlugin are
        // carried through so a `pure-js-npm` graph never runs as
        // `javascript-only` and a single-source prepare is never re-parsed.
        value.graphJson, value.entrypoint, value.profile, JSON.stringify(grants), nativeOptions,
      ));
    } catch (error) {
      if (error instanceof SandboxError && error.kind === 'invalid_configuration' && /unknown host callback/.test(error.message)) {
        throw new SandboxError(new Error('invalid_configuration: unknown host callback'));
      }
      throw error;
    }
    return new Sandbox(handle, this.#controllers);
  }
  /**
   * Attaches a trusted host directory to a live sandbox. All arguments are
   * trusted host values. The Rust facade retains the directory capability and
   * the sandbox policy must allow the virtual `prefix`.
   */
  mountDirectory(sandbox, prefix, path, access = 'read') {
    if (!(sandbox instanceof Sandbox)) throw new TypeError('sandbox must be a Sandbox');
    if (typeof prefix !== 'string' || typeof path !== 'string') throw new TypeError('prefix and path must be strings');
    if (!ACCESS.has(access)) throw new RangeError(`invalid_configuration: unknown mount access '${access}'`);
    sync(() => this.#native.mountDirectory(sandbox._handle(), prefix, path, access));
  }
  get activeSandboxes() { return this.#native.activeSandboxes; }
  /** Engine hard limits as configured, enforced by the facade on every create. */
  get limits() { return Object.freeze({...this.#native.limits}); }
  shutdown() {
    for (const active of this.#controllers.values()) for (const controller of active) controller.abort();
    return asyncCall(() => this.#native.shutdown());
  }
  async [Symbol.asyncDispose]() { if (!await this.shutdown()) throw new SandboxError(new Error("incomplete_close: host work still owns resources")); }
}
class Sandbox {
  #native;
  #controllers;
  #report;
  constructor(handle, controllers) { this.#native = handle; this.#controllers = controllers; }
  _handle() { return this.#native; }
  get id() { return this.#native.id; }
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
  /**
   * Resolves to the facade's report. A report that is not clean (for example
   * pending host callbacks that still own resources) is cached and returned
   * again by later close() calls instead of being upgraded to success.
   */
  close() {
    this.#abortCallbacks();
    if (this.#report?.closedCleanly) return Promise.resolve(this.#report);
    return asyncCall(async () => {
      const report = Object.freeze(await this.#native.close());
      if (report.closedCleanly) this.#report = report;
      return report;
    });
  }
  async [Symbol.asyncDispose]() {
    const report = await this.close();
    if (!report.closedCleanly) throw new SandboxError(new Error(`incomplete_close: ${report.pendingHostOperations} host operations still pending`));
  }
}
module.exports = {Engine, PreparedPlugin, Sandbox, SandboxError};
