import {
  entity,
  role,
  uuid,
  text,
  email,
  set,
  int,
  decimal,
  boolean,
  date,
  one,
  many,
} from '@microsoft/rayfin-core';
import { Category } from './Category.js';
import { Comment } from './Comment.js';

/**
 * Every scalar decorator and option, an explicitly declared FK column
 * (`category_id`) for an optional `@one`, the parent side of `Comment.todo`,
 * and per-action rules with field include/exclude and policies.
 */
@entity()
@role('anonymous', 'read', {
  include: ['id', 'title', 'status', 'dueDate', 'category_id'],
  policy: (_claims, item) => item.isPublic.eq(true),
})
@role('authenticated', 'create', {
  policy: (claims, item) => claims.sub.eq(item.user_id),
})
@role('authenticated', 'read', {
  exclude: ['secretNote'],
  policy: (claims, item) => claims.sub.eq(item.user_id).or(item.isPublic.eq(true)),
})
@role('authenticated', ['update', 'delete'], {
  include: ['title', 'status', 'priority', 'estimate', 'weight', 'done', 'dueDate', 'secretNote'],
  exclude: ['secretNote'],
  policy: (claims, item) =>
    item.user_id
      .eq(claims.sub)
      .and(item.status.neq("it's archived").or(item.estimate.eq(null)).or(item.estimate.neq(0))),
})
export class Todo {
  @uuid() id!: string;
  @text() user_id!: string;
  @text({ min: 1, max: 200 }) title!: string;
  @text({ optional: true, unique: true, max: 80, regex: /^[a-z0-9-]+$/i }) slug?: string;
  @email({ optional: true, unique: true }) contactEmail?: string;
  @set({ default: "it's new" }, "it's new", 'doing', 'done', "it's archived")
  status!: "it's new" | 'doing' | 'done' | "it's archived";
  @set({ enum: ['low', 'high'], optional: true }) priority?: 'low' | 'high';
  @int({ optional: true, min: 0 }) estimate?: number;
  @decimal() cost!: number;
  @decimal({ precision: 10, scale: 4, optional: true, min: 0, max: 99999 }) weight?: number;
  @boolean({ default: false }) isPublic!: boolean;
  @boolean({ optional: true }) done?: boolean;
  @date() createdAt!: Date;
  @date({ optional: true }) dueDate?: Date;
  @uuid({ optional: true, unique: true }) externalRef?: string;
  @text({ optional: true }) secretNote?: string;
  @uuid({ optional: true }) category_id?: string;
  @one(() => Category, { optional: true }) category?: Category;
  @many(() => Comment) comments?: Comment[];
}
