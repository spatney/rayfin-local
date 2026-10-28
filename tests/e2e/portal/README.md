# Sign-in popup end-to-end test

Drives rayfin-local's sign-in portal (the popup `ensureSignedInWithFabric` opens) with a real
browser and the real Rayfin SDK 1.36.2. `crates/rl-auth/tests/portal_e2e.rs` starts the auth
routes behind `rl-server` and runs `run.mjs`, which:

1. bundles `app.js` (a Sign in button calling `ensureSignedInWithFabric`) with esbuild, taking
   `@microsoft/rayfin-client` and `@microsoft/rayfin-auth-provider-fabric` from an app's
   `node_modules`;
2. serves it from its own origin, as Vite serves an app next to the backend;
3. clicks Sign in in Chromium (Playwright) and, in the popup: signs in with a wrong then the
   right password, signs up, uses "Continue as" (with a fake developer token), cancels, and
   closes the popup; it also opens the portal without an opener;
4. prints `RESULT <json>` with what the app saw (session, refresh, errors), which the Rust test
   checks.

## Run it

```powershell
cd tests/e2e/portal
npm install
npx playwright install chromium   # optional: without it, the installed Microsoft Edge is used
cd ../../..
cargo test -p rl-auth --test portal_e2e -- --ignored --nocapture
```

The SDK comes from `RAYFIN_SDK_NODE_MODULES`; the default is the explorer app checked out next to
this repository (`../fabric-apps/apps/explorer/node_modules`). The test prints `SKIPPED` and
passes when Playwright or the SDK is missing.

`node_modules/` and the `.build/` bundle are not committed.

## The Entra exchange with a real token

`crates/rl-auth/tests/entra_smoke.rs` mints your Power BI token with the Azure CLI (`az login`
first), exchanges it at `POST /api/auth/v1/brokered/token`, which checks it against Microsoft's
published signing keys, and asks the sign-in page who "Continue as" would sign in. It prints no
token and no identity:

```powershell
cargo test -p rl-auth --test entra_smoke -- --ignored --nocapture
```
