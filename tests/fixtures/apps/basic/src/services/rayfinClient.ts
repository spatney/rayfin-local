import { ConnectorsRayfinClient } from '@microsoft/rayfin-client';
import {
  fabricSemanticModel,
  type FabricSemanticModel,
} from '@microsoft/rayfin-connector-fabric-semanticmodel';
import type { ConnectorConfig } from '@microsoft/rayfin-connectors';
import { createStorageClient, type StorageClient } from '@microsoft/rayfin-storage';

import type { BasicSchema } from '../../rayfin/data/schema';
import type { AppFunctionsSchema } from '../../rayfin/functions/src/types';
import type { Attachments } from '../../rayfin/storage/Attachments';
import type { Avatars } from '../../rayfin/storage/Avatars';

/*
 * `rayfin connector add` would generate the connector schema and config below in
 * rayfin/connectors/salesmodel/schema.ts. They live here because everything under rayfin/
 * (other than functions/) is compiled into crates/rl-model's golden analysis of this fixture.
 */

/** Connectors declared in rayfin/rayfin.yml, keyed by their exact `name`. */
export type AppConnectorsSchema = {
  salesmodel: FabricSemanticModel<'executeQuery'>;
};

const connectorConfigs = {
  salesmodel: { connector: 'fabric-semanticmodel' },
} as const satisfies Record<keyof AppConnectorsSchema, ConnectorConfig>;

/** Storage folders (rayfin/storage), keyed by their normalized `@blob` names. */
export type AppStorageSchema = {
  'todo-attachments': Attachments;
  'user-avatars': Avatars;
};

export interface AppClientConfig {
  baseUrl: string;
  publishableKey: string;
  /** Local-debugging override for the functions route. */
  functionsBaseUrl?: string;
}

/**
 * Data, auth, functions and connectors from `ConnectorsRayfinClient`, plus `storage` on the
 * same API client (and so the same session), as `ExtendableRayfinClient` composes it.
 */
export class AppClient extends ConnectorsRayfinClient<
  BasicSchema,
  AppFunctionsSchema,
  AppConnectorsSchema
> {
  readonly storage: StorageClient<AppStorageSchema>;

  constructor(config: AppClientConfig) {
    super(
      {
        baseUrl: config.baseUrl,
        publishableKey: config.publishableKey,
        useProxy: false,
        authStorage: true,
        functionsBaseUrl: config.functionsBaseUrl,
        connectors: connectorConfigs,
      },
      // Required: the semantic-model runtime decodes the Arrow response.
      { salesmodel: fabricSemanticModel() },
    );
    this.storage = createStorageClient<AppStorageSchema>(this.apiClient);
  }
}
