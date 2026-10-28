import { role } from '@microsoft/rayfin-core';
import { blob } from '@microsoft/rayfin-core/experimental';

/** The string form of `@blob` with defaults, and an action without a policy. */
@blob('user-avatars')
@role('authenticated', 'read')
@role('authenticated', ['create', 'delete'], {
  policy: (claims, item) => claims.sub.eq(item.owner_id),
})
export class Avatars {
  declare owner_id: string;
}
