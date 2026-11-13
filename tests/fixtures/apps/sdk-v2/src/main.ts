import { RayfinClient, resolveRayfinConfig } from '@microsoft/rayfin-client';
import { browserSession } from '@microsoft/rayfin-auth';
import { dataService } from '@microsoft/rayfin-data';
import { functionsService } from '@microsoft/rayfin-functions';
import { storageService } from '@microsoft/rayfin-storage';
import type { AppSchema } from '../rayfin/data/schema.js';

const config = await resolveRayfinConfig({
  apiUrl: import.meta.env.VITE_RAYFIN_API_URL,
  publishableKey: import.meta.env.VITE_RAYFIN_PUBLISHABLE_KEY,
});
if (typeof config.baseUrl !== 'string' || !config.publishableKey) {
  throw new Error('The local Rayfin runtime configuration is missing.');
}

export const client = RayfinClient.create({
  baseUrl: config.baseUrl,
  publishableKey: config.publishableKey,
  runtimeConfig: config.runtimeConfig,
  auth: browserSession(),
  services: {
    data: dataService<AppSchema>(),
    functions: functionsService<{
      greet: { input: { name: string }; output: string };
      countTodos: { input: Record<string, never>; output: { count: number; hasCallerToken: boolean } };
    }>(),
    storage: storageService<{ attachments: { description?: string } }>(),
  },
});
