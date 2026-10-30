// The session stamp of rayfin-local's dev server pages (imported by vite-dev.mjs, which puts
// `sessionStampScript(stamp)` first in each page's <head>).
//
// Pages keep the SDK's session (`authSession` in localStorage, with `authStorage: true`) per
// origin, but the dev server's port serves another app over time, and the backend signs every
// session out on a data reset or a new sign-in identity. The SDK trusts a stored session until
// it expires, so the app would get 401s for it. So before the app's code runs, each page compares
// the origin's stamp with the backend's, `<appId>:<sessionEpoch>`, and forgets the SDK's stored
// session when they differ.

/** The localStorage key of the stamp of the backend whose sessions the origin's storage holds. */
export const SESSION_STAMP_KEY = 'rayfin-local:session-stamp';

/**
 * Forgets the SDK's stored session and sign-in state unless `storage` is stamped with `stamp`,
 * then stamps it; other keys stay. Returns whether it forgot them.
 *
 * @microsoft/rayfin-auth 1.36 keeps the session in `authSession` and PKCE states in
 * `rayfin_pkce_<state>`, both after `<storageKeyPrefix>_` when the app sets a prefix.
 *
 * Runs in the page as its source text, so it uses nothing from outside.
 */
export function forgetStaleSession(storage, stamp, stampKey) {
  if (storage.getItem(stampKey) === stamp) return false;
  const sdkKey = /^(?:.+_)?(?:authSession$|rayfin_pkce_)/;
  const stale = [];
  for (let index = 0; index < storage.length; index += 1) {
    const key = storage.key(index);
    if (key !== null && sdkKey.test(key)) stale.push(key);
  }
  for (const key of stale) storage.removeItem(key);
  storage.setItem(stampKey, stamp);
  return true;
}

/** The inline script that runs `forgetStaleSession` on the page's localStorage. */
export function sessionStampScript(stamp) {
  const literal = (value) => JSON.stringify(String(value)).replace(/</g, '\\u003c');
  return (
    `try{(${forgetStaleSession})(window.localStorage,${literal(stamp)},` +
    `${literal(SESSION_STAMP_KEY)})}catch{}`
  );
}
