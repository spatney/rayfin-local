/**
 * Secret names declared in rayfin.yml, in the form `rayfin secret set` generates.
 */

declare module '@microsoft/fabric-user-data-functions' {
  interface RayfinSecretRegistry {
    API_KEY: string;
  }
}

export {};
