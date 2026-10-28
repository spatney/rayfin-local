import { entity, role, uuid, text, int, one, many } from '@microsoft/rayfin-core';
import { Todo } from './Todo.js';

/**
 * Simple per-action rules without policies, a renamed column, a unique bounded
 * text field, an int with a default and a range, a self-referencing optional
 * `@one`, and the collection side of `Todo.category`.
 */
@entity()
@role('anonymous', 'read')
@role('authenticated', ['create', 'read', 'update', 'delete'])
export class Category {
  @uuid() id!: string;
  @text({ unique: true, min: 2, max: 100 }) name!: string;
  @text({ column: 'display_label', optional: true, max: 200 }) label?: string;
  @int({ default: 0, min: 0, max: 1000 }) sortOrder!: number;
  @one(() => Category, { optional: true }) parent?: Category;
  @many(() => Todo) todos?: Todo[];
}
