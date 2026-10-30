import { date, entity, role, text } from '@microsoft/rayfin-core';
import { Source } from '@microsoft/rayfin-connectors';

// The table has no primary key: a keyless entity (no findByKey, update or delete).
@role('authenticated', ['read'])
@entity()
export class AuditEntry extends Source({ schema: 'dbo', table: 'audit_log', primaryKey: [] }) {
  @date()
  at!: Date;

  @text({ max: 50 })
  action!: string;

  @text({ optional: true, max: 400 })
  detail?: string;
}
