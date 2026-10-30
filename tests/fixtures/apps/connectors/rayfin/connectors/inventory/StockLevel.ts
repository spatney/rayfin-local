import { entity, int, one, role, text, uuid } from '@microsoft/rayfin-core';
import { Source } from '@microsoft/rayfin-connectors';
import { Product } from './Product.js';

@role('authenticated', ['read', 'create', 'update', 'delete'])
@entity()
export class StockLevel extends Source({
  schema: 'sales',
  table: 'stock_levels',
  primaryKey: ['productId', 'warehouseCode'],
}) {
  @uuid({ column: 'product_id' })
  productId!: string;

  @text({ column: 'warehouse_code', max: 10 })
  warehouseCode!: string;

  @int()
  quantity!: number;

  @one(() => Product, { sourceFields: ['productId'], targetFields: ['productId'] })
  product!: Product;
}
