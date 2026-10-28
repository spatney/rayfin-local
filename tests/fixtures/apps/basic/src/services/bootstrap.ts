import type { FabricAuthOptions } from '@microsoft/rayfin-auth-provider-fabric';
import { resolveRayfinFunctionsBaseUrl } from '@microsoft/rayfin-local-dev';

import { AppClient } from './rayfinClient';

export interface AppServices {
  client: AppClient;
  fabricOptions: FabricAuthOptions;
}

/**
 * Reads the VITE_* variables (written by `rayfin env`, or set by `rayfin-local serve`) and
 * creates the Rayfin client and the Fabric sign-in options.
 */
export function bootstrap(): AppServices {
  const apiUrl = import.meta.env.VITE_RAYFIN_API_URL;
  const publishableKey = import.meta.env.VITE_RAYFIN_PUBLISHABLE_KEY;
  if (!apiUrl) {
    throw new Error('VITE_RAYFIN_API_URL environment variable is required');
  }
  if (!publishableKey) {
    throw new Error('VITE_RAYFIN_PUBLISHABLE_KEY environment variable is required');
  }

  const workspaceId = import.meta.env.VITE_FABRIC_WORKSPACE_ID;
  const projectId = import.meta.env.VITE_FABRIC_ITEM_ID;
  const fabricPortalUrl = import.meta.env.VITE_FABRIC_PORTAL_URL;
  if (!workspaceId || !projectId || !fabricPortalUrl) {
    throw new Error(
      'Missing required Fabric config. Set VITE_FABRIC_WORKSPACE_ID, VITE_FABRIC_ITEM_ID, and VITE_FABRIC_PORTAL_URL.',
    );
  }

  const client = new AppClient({
    baseUrl: apiUrl.endsWith('/') ? apiUrl : `${apiUrl}/`,
    publishableKey,
    functionsBaseUrl: resolveRayfinFunctionsBaseUrl(),
  });
  return {
    client,
    fabricOptions: {
      workspaceId,
      projectId,
      fabricPortalUrl,
      returnOrigin: window.location.origin,
    },
  };
}
