/**
 * The gateway endpoint for a workspace image (`GET /api/workspace/image`).
 *
 * Frozen by step 03: `session_id` names the session whose workspace the path is
 * resolved against, `path` is the file (relative to that workspace, or absolute
 * inside it). The gateway answers bytes on 200 and 400/403/404/413 otherwise — all
 * of which mean the same thing here ("not renderable", see `resolver.ts`).
 */

/** `${base}/api/workspace/image?session_id=…&path=…` (base without its trailing slash). */
export function workspaceImageUrl(baseUrl: string, sessionId: string, path: string): string {
  const base = baseUrl.endsWith('/') ? baseUrl.slice(0, -1) : baseUrl;
  const query = new URLSearchParams({ session_id: sessionId, path });
  return `${base}/api/workspace/image?${query.toString()}`;
}
