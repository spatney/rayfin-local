import { role, text, int } from '@microsoft/rayfin-core';
import { blob, StorageObject, mb, ContentTypes } from '@microsoft/rayfin-core/experimental';

/**
 * A storage folder with every `@blob` option, app fields stored in
 * `user_metadata`, and per-action rules with policies over intrinsic and app
 * fields.
 */
@blob({
  name: 'Todo Attachments',
  onConflict: 'overwrite',
  maxSize: mb(5),
  allowedContentTypes: [ContentTypes.AnyImage, 'application/pdf'],
})
@role('authenticated', ['create', 'read', 'update'], {
  policy: (claims, item) => claims.sub.eq(item.owner_id),
})
@role('authenticated', 'delete', {
  policy: (claims, item) => claims.sub.eq(item.owner_id).and(item.team_id.neq(null)),
})
export class Attachments extends StorageObject {
  @text() team_id!: string;
  @int({ optional: true }) pages?: number;
}
