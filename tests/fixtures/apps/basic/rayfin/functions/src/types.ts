/**
 * Function schema types for RayfinClient.
 *
 * AUTO-GENERATED — do not edit manually.
 * Re-generated automatically when function source files change.
 *
 * If this file is not updating automatically, run:
 *   rayfin dev functions apply
 *
 * The schema is a closed object type: only the function names listed
 * below are accepted by RayfinClient.functions.<name>.invoke(...).
 * Adding, renaming, or changing the signature of a udf.func() call
 * regenerates this file and surfaces type errors at every consumer.
 *
 * IMPORTANT: This file must NOT import any Node.js packages — it is
 * resolved by the frontend app's TypeScript compiler.
 */

export type AppFunctionsSchema = {
  ping: {
    input: Record<string, never>;
    output: string;
  };
  greet: {
    input: { name: string; excited: boolean };
    output: string;
  };
  echo: {
    input: { payload: null | { a: number; tags: string[] } };
    output: { payload: null | { a: number; tags: string[] }; tagCount: number };
  };
  whoAmI: {
    input: Record<string, never>;
    output: { hasCallerToken: boolean; baseUrl: string };
  };
  countTodos: {
    input: Record<string, never>;
    output: { count: number };
  };
  tokenInfo: {
    input: Record<string, never>;
    output: { fabricTokenLength: number };
  };
  secretInfo: {
    input: Record<string, never>;
    output: { matchesFixture: boolean; length: number };
  };
  fail: {
    input: { message: string };
    output: never;
  };
};
