import { entity, role, uuid, text, email, date, one } from '@microsoft/rayfin-core';
import { Todo } from './Todo.js';

/**
 * A required `@one` whose FK column (`todo_id`) is generated, not declared,
 * and policies over `claims.email` with nested and/or.
 */
@entity()
@role('anonymous', 'read', { exclude: ['authorEmail'] })
@role('authenticated', 'read')
@role('authenticated', ['create', 'update', 'delete'], {
  policy: (claims, item) =>
    claims.email
      .eq(item.authorEmail)
      .and(claims.sub.eq(item.author_id).or(claims.role.eq('authenticated'))),
})
export class Comment {
  @uuid() id!: string;
  @text() author_id!: string;
  @email({ min: 6 }) authorEmail!: string;
  @text({ max: 2000 }) body!: string;
  @date() createdAt!: Date;
  @one(() => Todo) todo?: Todo;
}
