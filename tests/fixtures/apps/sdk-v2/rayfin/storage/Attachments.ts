import { blob, role, StorageObject, text } from '@microsoft/rayfin-core';

@blob('attachments')
@role('authenticated', '*', {
  policy: (claims, item) => claims.sub.eq(item.owner_id),
})
export class Attachments extends StorageObject {
  @text({ optional: true }) description?: string;
}
