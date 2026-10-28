# rayfin-local

Run a [Rayfin](https://rayfin.ai) app, including its whole backend, on your own machine:

```text
cd my-rayfin-app
npm install
rayfin-local serve
```

`rayfin-local` reads your project (`rayfin/rayfin.yml`, the entities in `rayfin/data`, your functions and connectors) and starts local stand-ins for every Rayfin service:

- a SQLite database
- a DAB-compatible GraphQL data API with your `@role` policies
- username/password sign-in, including the "Sign in with Fabric" popup
- your functions
- semantic-model connectors
- storage

It then starts your Vite dev server pointed at all of that.
Nothing is deployed to Fabric, and your app code doesn't change.

> Status: in development.
