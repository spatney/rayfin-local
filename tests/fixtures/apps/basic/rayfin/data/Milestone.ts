import { entity, text, date } from '@microsoft/rayfin-core';

/**
 * No `id` (the analyzer adds an implicit UUID primary key) and no `@role`
 * (the generated config defaults to `authenticated: *`).
 */
@entity()
export class Milestone {
  @text({ max: 120 }) title!: string;
  @date({ optional: true }) due?: Date;
}
