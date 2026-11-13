import { entity, int, role, text, uuid } from '@microsoft/rayfin-core';
import { Source } from '@microsoft/rayfin-connectors';

@entity()
@role('authenticated', 'read', {
  include: ['productId', 'title', 'quantity'],
  exclude: ['internalTitle'],
  policy: (claims, item) => claims.sub.eq(item.ownerId),
})
export class Product extends Source({
  schema: 'inventory',
  table: 'products',
  primaryKey: ['productId'],
}) {
  @uuid({ column: 'product-id' }) productId!: string;
  @text({ column: 'product-title' }) title!: string;
  @text({ column: 'title' }) internalTitle!: string;
  @text({ column: 'owner-id' }) ownerId!: string;
  @int() quantity!: number;
}
