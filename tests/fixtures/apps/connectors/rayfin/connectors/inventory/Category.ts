import { entity, int, many, role, text } from '@microsoft/rayfin-core';
import { Source } from '@microsoft/rayfin-connectors';
import { Product } from './Product.js';

@role('anonymous', ['read'], { include: ['categoryId', 'name'] })
@role('authenticated', ['read'])
@entity()
export class Category extends Source({ schema: 'dbo', table: 'categories', primaryKey: ['categoryId'] }) {
  @int({ column: 'category_id' })
  categoryId!: number;

  @text({ max: 100 })
  name!: string;

  @text({ optional: true, max: 400 })
  description?: string;

  @many(() => Product, { sourceFields: ['categoryId'], targetFields: ['categoryId'] })
  products!: Product[];
}
