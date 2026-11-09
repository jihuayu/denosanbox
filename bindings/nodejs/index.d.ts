export type Json = null | boolean | number | string | Json[] | {[key: string]: Json};
export class SandboxError extends Error { readonly kind: string }
export class PreparedPlugin { private constructor() }
export class Engine {
  constructor();
  prepare(source: string, entrypoint?: string): PreparedPlugin;
  create(plugin: PreparedPlugin): Promise<Sandbox>;
  shutdown(): Promise<boolean>;
  [Symbol.asyncDispose](): Promise<void>;
}
export class Sandbox {
  private constructor();
  call(exportName: string, input: Json, options?: {timeoutMs?: number}): Promise<Json>;
  cancel(): void;
  close(): Promise<boolean>;
  [Symbol.asyncDispose](): Promise<void>;
}
