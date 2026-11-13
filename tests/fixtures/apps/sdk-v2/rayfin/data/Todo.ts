import { boolean, date, entity, int, role, text, uuid } from '@microsoft/rayfin-core';

@entity()
@role('authenticated', '*', {
  policy: (claims, item) => claims.sub.eq(item.user_id),
})
export class Todo {
  @uuid() id!: string;
  @text() user_id!: string;
  @text({ min: 1, max: 200 }) title!: string;
  @boolean({ default: false }) done!: boolean;
  @int({ default: 0 }) estimate!: number;
  @date({ optional: true }) createdAt?: Date;
}
