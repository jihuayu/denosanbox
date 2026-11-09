export type Json = null | boolean | number | string | Json[] | {[key: string]: Json};

/** Stable error categories mirror the Rust facade's `Error::kind()`. */
export type SandboxErrorKind =
  | 'runtime_unavailable'
  | 'unsupported_feature'
  | 'unsupported_language'
  | 'invalid_plugin'
  | 'module_not_found'
  | 'module_denied'
  | 'limit_exceeded'
  | 'queue_full'
  | 'closed'
  | 'cancelled'
  | 'deadline_exceeded'
  | 'execution'
  | 'backend'
  | 'policy'
  | 'invalid_configuration'
  | 'internal'
  | 'incomplete_close'
  | (string & {});

export class SandboxError extends Error { readonly kind: SandboxErrorKind }

/** Explicit compatibility profile. No profile grants VFS/env/network itself. */
export type CompatibilityProfile = 'javascript-only' | 'pure-js-npm';

export type MountAccess = 'read' | 'read-write';

export interface EngineOptions {
  /** Facade capability names authorized by the immutable engine ceiling. */
  hostCapabilities?: string[];
  /** Virtual read/write roots authorized by the engine ceiling. */
  readRoots?: string[];
  writeRoots?: string[];
  /** JS heap target of one isolate; a target, not an RSS ceiling. */
  heapTargetBytes?: number;
  /** Aggregate managed-memory budget for a sandbox and its descendants. */
  memoryBudgetBytes?: number;
  maxSandboxes?: number;
  maxMessageBytes?: number;
  /** Per-invocation policy deadline in milliseconds. */
  deadlineMs?: number;
  queueDepth?: number;
  maxLogBytes?: number;
  // Engine hard limits (facade `EngineLimits`), enforced on every create.
  maxActiveSandboxes?: number;
  maxPendingInvocations?: number;
  /** Engine hard JSON message limit (distinct from policy `maxMessageBytes`). */
  engineMessageBytes?: number;
  defaultInvocationTimeoutMs?: number;
  maxInvocationTimeoutMs?: number;
  maxModules?: number;
  maxModuleBytes?: number;
  maxGraphBytes?: number;
}

/** Read-only engine hard limits as configured (`EngineLimits`). */
export interface EngineLimits {
  readonly maxActiveSandboxes: number;
  readonly maxPendingInvocations: number;
  readonly maxMessageBytes: number;
  readonly defaultInvocationTimeoutMs: number;
  readonly maxInvocationTimeoutMs: number;
  readonly maxModules: number;
  readonly maxModuleBytes: number;
  readonly maxGraphBytes: number;
}

export interface SandboxCreateOptions {
  defaultTimeoutMs?: number;
  memoryLimitBytes?: number;
  /** Per-invocation policy deadline in milliseconds. */
  deadlineMs?: number;
  queueDepth?: number;
  maxMessageBytes?: number;
  maxLogBytes?: number;
  /** Requested capabilities; must be covered by the engine ceiling. */
  capabilities?: string[];
  /** Requested virtual read/write roots; must be covered by the ceiling. */
  readRoots?: string[];
  writeRoots?: string[];
  /** Callback name to trusted host scope. The scope is never guest-controlled. */
  hostGrants?: Record<string, string>;
}

export interface CloseReport {
  /** False while host callback work still owns resources. */
  readonly closedCleanly: boolean;
  readonly forcedTermination: boolean;
  readonly invocationsCompleted: number;
  readonly invocationsCancelled: number;
  readonly pendingHostOperations: number;
}

export class PreparedPlugin {
  private constructor();
  readonly entrypoint: string;
  /** Sorted root-relative module paths of the immutable facade graph. */
  readonly modulePaths: readonly string[];
  readonly profile: CompatibilityProfile;
}

export class Engine {
  constructor(options?: EngineOptions);
  registerHost(name: string, callback: (context: InvocationContext, input: Json) => Json | Promise<Json>, options?: {inputLimit?: number; outputLimit?: number}): void;
  /** Single-module graph under the `javascript-only` profile. */
  prepare(source: string, entrypoint?: string): PreparedPlugin;
  /**
   * Multi-module graph. `modules` maps root-relative `.js`/`.mjs`/`.ts`/`.mts`
   * paths to source; imports and guest workers resolve only inside the map.
   * The validated `profile` (default `'javascript-only'`) is stored on the
   * returned PreparedPlugin and used verbatim by `create`; unknown names throw
   * `RangeError('invalid_configuration: ...')` and are never downgraded.
   */
  prepareModules(modules: Record<string, string>, entrypoint?: string, profile?: CompatibilityProfile): PreparedPlugin;
  create(plugin: PreparedPlugin, options?: SandboxCreateOptions): Promise<Sandbox>;
  /** Trusted host directory mount; the sandbox policy must allow `prefix`. */
  mountDirectory(sandbox: Sandbox, prefix: string, path: string, access?: MountAccess): void;
  readonly activeSandboxes: number;
  readonly limits: EngineLimits;
  shutdown(): Promise<boolean>;
  [Symbol.asyncDispose](): Promise<void>;
}
export class Sandbox {
  private constructor();
  readonly id: string;
  call(exportName: string, input: Json, options?: {timeoutMs?: number; signal?: AbortSignal}): Promise<Json>;
  cancel(): void;
  close(): Promise<CloseReport>;
  [Symbol.asyncDispose](): Promise<void>;
}

export interface InvocationContext {
  readonly sandboxId: string;
  readonly scope: string;
  readonly remainingMs: number | null;
  readonly signal: AbortSignal;
}
