import { entity, one, role, text, uuid } from '@microsoft/rayfin-core';
import { Todo } from './Todo.js';

@entity()
@role('authenticated', '*', {
  policy: (claims, item) => claims.sub.eq(item.user_id),
})
export class Comment {
  @uuid() id!: string;
  @text() user_id!: string;
  @text() content!: string;
  @one(() => Todo) todo!: Todo;
}
