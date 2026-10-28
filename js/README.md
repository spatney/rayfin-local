# JavaScript helpers

These ES modules are compiled into the `rayfin-local` binary with `include_str!` and written to
`<project>/.rayfin-local/cache/js/<name>-<hash>.mjs` before they run (`rl_node::Helper`).
They run on the developer's own Node.js (20 or later), with the project directory as the working
directory, so they can import the project's installed packages (`@microsoft/rayfin-core`,
`typescript`, `@microsoft/rayfin-cli`, ...).

Rules:

- Plain ESM (`.mjs`), no dependencies of their own: only Node built-ins and the project's packages.
- Resolve project packages from the project, not from the helper's location, for example with
  `createRequire(path.join(projectRoot, 'package.json'))` or `import.meta.resolve` relative to the project.
- One-shot helpers follow the `rl_node::run_helper` protocol: read JSON input from the file named by
  `--input <file>` and write exactly one JSON document to the file named by `--output <file>`:
  `{"ok": true, "result": ...}` or `{"ok": false, "error": {"message": "...", "stack": "..."}}`.
  Stdout and stderr are free for logs (user modules may print while being imported).
- Long-running helpers (the function host) document their own protocol next to their Rust owner.

| Helper | Owner crate | Purpose |
| --- | --- | --- |
| `analyze.mjs` | `rl-model` | Compile `rayfin/data` and run `@microsoft/rayfin-core`'s analyzer. |
| `rayfin-token.mjs` | `rl-tokens` | Mint tokens with the Rayfin CLI's login (`@microsoft/rayfin-cli/auth`). |
| `udf-metadata.mjs` | `rl-functions` | Generate `runtimemetadata.json` from `udf.func(...)` declarations. |
| `function-host.mjs` | `rl-functions` | Host the app's functions on the real UDF package, without Core Tools. |
