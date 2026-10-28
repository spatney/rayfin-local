import { entity, role, uuid, text, int, decimal, boolean, date, one, User } from '@microsoft/rayfin-core';

/**
 * An explicit entity name with an irregular plural (`Person` → `People`), a
 * `@one` to the system `User` entity, and defaults of every scalar kind.
 */
@entity('Person')
@role('authenticated', '*', {
  policy: (claims, item) => claims.sub.eq(item.account_id),
})
export class PersonRecord {
  @uuid() id!: string;
  @text({ default: 'n/a', max: 50 }) nickname!: string;
  @int({ default: 1 }) level!: number;
  @decimal({ default: 9.99 }) rate!: number;
  @boolean({ default: true }) active!: boolean;
  @date({ default: new Date('2024-01-01T00:00:00.000Z') }) memberSince!: Date;
  @uuid({ optional: true }) account_id?: string;
  @one(() => User, { optional: true }) account?: User;
}
