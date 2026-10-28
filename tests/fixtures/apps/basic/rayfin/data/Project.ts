import { entity, role, text, decimal, date, many } from '@microsoft/rayfin-core';
import { Milestone } from './Milestone.js';

/**
 * A unidirectional `@many` (Milestone has no `@one` back, so the analyzer adds
 * a NOT NULL `Project_id` column to Milestones) and policies with date, number,
 * null and boolean literals.
 */
@entity()
@role('anonymous', 'read', {
  policy: (_claims, item) =>
    item.startsOn.neq(new Date('2000-01-01T00:00:00.000Z')).and(item.budget.neq(0.5)),
})
@role('authenticated', '*', {
  policy: (claims, item) => item.owner_id.eq(claims.sub).or(item.closedAt.eq(null)),
})
export class Project {
  @text() owner_id!: string;
  @text({ max: 120 }) name!: string;
  @decimal({ precision: 12, scale: 2, optional: true }) budget?: number;
  @date({ optional: true }) startsOn?: Date;
  @date({ optional: true }) closedAt?: Date;
  @many(() => Milestone) milestones?: Milestone[];
}
