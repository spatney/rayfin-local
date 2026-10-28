// Drives the real @microsoft/rayfin-storage 1.36.2 client against rl-storage (run by ../sdk.rs,
// which serves the `basic` fixture's folders and accepts the bearer token RL_TOKEN).
import assert from 'node:assert/strict';
import { ExtendableRayfinClient } from '@microsoft/rayfin-client/experimental';
import { createStorageClient, isStorageError } from '@microsoft/rayfin-storage';

const { RL_BASE_URL: baseUrl, RL_PUBLISHABLE_KEY: publishableKey, RL_TOKEN: token } = process.env;
const services = { storage: createStorageClient };
const client = ExtendableRayfinClient.create({ baseUrl, publishableKey, services });
// The browser client's `Auth` supplies the signed-in session's token; stand in for a session.
client.apiClient.setAccessTokenCallback(() => token);
const attachments = client.storage['todo-attachments'];

const pdf = new TextEncoder().encode('%PDF-1.4 hello');
const options = { prefix: 'team', contentType: 'application/pdf', fields: { team_id: 't1', pages: 2 } };
const uploaded = await attachments.upload('report.pdf', pdf, options);
assert.equal(uploaded.object.path, 'team/report.pdf');

const listed = await attachments.list({ prefix: 'team' });
assert.equal(listed.items.length, 1);
const [item] = listed.items;
assert.equal(item.path, 'team/report.pdf');
assert.equal(item.size, pdf.byteLength);
assert.equal(item.team_id, 't1');
assert.equal(item.pages, 2);
assert.equal(listed.continuation, undefined);

const downloaded = await attachments.download('report.pdf', { prefix: 'team' });
assert.equal(await new Response(downloaded.stream).text(), '%PDF-1.4 hello');
assert.equal(downloaded.contentType, 'application/pdf');
assert.equal(downloaded.contentLength, pdf.byteLength);
assert.equal(downloaded.etag, item.etag);

const deleted = await attachments.delete('report.pdf', { prefix: 'team' });
assert.equal(deleted.success, true);
assert.equal((await attachments.list()).items.length, 0);

// Bodies are streamed, not held to a request-size cap.
const big = new Uint8Array(3 * 1024 * 1024).fill(7);
await attachments.upload('big.pdf', big, { contentType: 'application/pdf', fields: { team_id: 't1' } });
const bigDownload = await attachments.download('big.pdf');
assert.equal((await new Response(bigDownload.stream).arrayBuffer()).byteLength, big.byteLength);
await attachments.delete('big.pdf');

const missing = await attachments.download('report.pdf', { prefix: 'team' }).catch((e) => e);
assert.ok(isStorageError(missing), String(missing));
assert.equal(missing.code, 'NotFound');
assert.equal(missing.status, 404);

// A stream has no size up front: it goes up chunked, without maxBytes.
const avatars = client.storage['user-avatars'];
const chunks = ['streamed ', 'bytes'];
const stream = new ReadableStream({
  pull(controller) {
    const chunk = chunks.shift();
    if (chunk === undefined) controller.close();
    else controller.enqueue(new TextEncoder().encode(chunk));
  },
});
await avatars.upload('me.txt', stream, { contentType: 'text/plain' });
const avatar = await avatars.download('me.txt');
assert.equal(await new Response(avatar.stream).text(), 'streamed bytes');

const anonymous = ExtendableRayfinClient.create({ baseUrl, publishableKey, services });
const denied = await anonymous.storage['user-avatars'].list().catch((e) => e);
assert.equal(denied.code, 'Unauthenticated');
assert.equal(denied.status, 401);

console.log('ok');
