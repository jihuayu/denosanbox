export type Json = null | boolean | number | string | Json[] | {[key: string]: Json};
export class SandboxError extends Error { readonly kind: string }
export class PreparedPlugin { private constructor() }
export class Engine {
  constructor(options?: {hostCapabilities?: string[]});
  registerHost(name: string, callback: (context: InvocationContext, input: Json) => Json | Promise<Json>, options?: {inputLimit?: number; outputLimit?: number}): void;
  prepare(source: string, entrypoint?: string): PreparedPlugin;
  create(plugin: PreparedPlugin, options?: {hostGrants?: Record<string, string>}): Promise<Sandbox>;
  shutdown(): Promise<boolean>;
  [Symbol.asyncDispose](): Promise<void>;
}
export class Sandbox {
  private constructor();
  call(exportName: string, input: Json, options?: {timeoutMs?: number; signal?: AbortSignal}): Promise<Json>;
  cancel(): void;
  close(): Promise<boolean>;
  [Symbol.asyncDispose](): Promise<void>;
}

export interface InvocationContext {
  readonly sandboxId: string;
  readonly scope: string;
  readonly remainingMs: number | null;
  readonly signal: AbortSignal;
}
