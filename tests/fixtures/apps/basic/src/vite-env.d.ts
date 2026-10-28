/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** The Rayfin API (the local backend under rayfin-local). */
  readonly VITE_RAYFIN_API_URL?: string;
  readonly VITE_RAYFIN_PUBLISHABLE_KEY?: string;
  /** `FabricAuthOptions.workspaceId`. */
  readonly VITE_FABRIC_WORKSPACE_ID?: string;
  /** `FabricAuthOptions.projectId`. */
  readonly VITE_FABRIC_ITEM_ID?: string;
  /** `FabricAuthOptions.fabricPortalUrl` (rayfin-local's sign-in portal). */
  readonly VITE_FABRIC_PORTAL_URL?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
