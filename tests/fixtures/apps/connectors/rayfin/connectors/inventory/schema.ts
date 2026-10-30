import type { GraphQLBackedConnector } from '@microsoft/rayfin-connector-fabric-graphql';
import type { ConnectorConfig } from '@microsoft/rayfin-connectors';

import { AuditEntry } from './AuditEntry.js';
import { Category } from './Category.js';
import { Product } from './Product.js';
import { StockLevel } from './StockLevel.js';

export { AuditEntry } from './AuditEntry.js';
export { Category } from './Category.js';
export { Product } from './Product.js';
export { StockLevel } from './StockLevel.js';

export const connectorConfig = {
  connector: 'fabric-sqldatabase',
  operations: ['read', 'create', 'update', 'delete'],
  entities: { AuditEntry, Category, Product, StockLevel },
} as const satisfies ConnectorConfig;

export type InventorySchema = GraphQLBackedConnector<
  {
    AuditEntry: typeof AuditEntry;
    Category: typeof Category;
    Product: typeof Product;
    StockLevel: typeof StockLevel;
  },
  typeof connectorConfig
>;
