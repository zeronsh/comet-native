/**
 * The /auth/* HTTP surface absorbed from comet's apps/server:
 *
 *  - POST /auth/exchange     — WorkOS code → tokens (see `workos.ts`).
 *  - POST /auth/refresh      — WorkOS refresh → fresh tokens (org-scopable).
 *  - GET  /auth/orgs         — the caller's active org memberships.
 *  - POST /auth/orgs         — create an org + first (admin) membership.
 *  - GET  /auth/cli/callback — headless sign-in: shows a paste-able code.
 *
 * Exchange/refresh/callback run BEFORE the bearer gate (the caller has no
 * access token yet); the org routes verify the bearer themselves — the user
 * id is ALWAYS the token's `sub`, never request input: users manage their own
 * memberships and no one else's. Error mapping matches the old server: bad
 * body 400, missing bearer 401, WorkOS-off 501, rejected exchange/refresh 401.
 */
import { bearerFromRequest, verifyToken } from "./auth";
import type { Env } from "./env";
import { WorkOsAuthFailed, createOrg, exchange, listOrgs, refresh } from "./workos";

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const notConfigured = (): Response => json({ error: "workos not configured" }, 501);

const authFailed = (e: unknown): Response =>
  json({ error: e instanceof WorkOsAuthFailed ? e.message : "authentication failed" }, 401);

const bodyJson = async <T>(request: Request): Promise<T | undefined> => {
  try {
    return (await request.json()) as T;
  } catch {
    return undefined;
  }
};

/** Handle an /auth/* route; undefined means "not an auth route". */
export const handleAuthRoute = async (
  request: Request,
  env: Env,
  url: URL
): Promise<Response | undefined> => {
  const parts = url.pathname.split("/").filter(Boolean);
  if (parts[0] !== "auth") return undefined;
  const apiKey = env.WORKOS_API_KEY;

  if (parts[1] === "exchange" && parts.length === 2 && request.method === "POST") {
    if (!apiKey) return notConfigured();
    const body = await bodyJson<{ code?: string }>(request);
    if (typeof body?.code !== "string") return json({ error: "missing code" }, 400);
    try {
      return json(await exchange(env, apiKey, body.code));
    } catch (e) {
      return authFailed(e);
    }
  }

  if (parts[1] === "refresh" && parts.length === 2 && request.method === "POST") {
    if (!apiKey) return notConfigured();
    const body = await bodyJson<{ refreshToken?: string; organizationId?: string }>(request);
    if (typeof body?.refreshToken !== "string") return json({ error: "missing refreshToken" }, 400);
    if (body.organizationId !== undefined && typeof body.organizationId !== "string") {
      return json({ error: "missing refreshToken" }, 400);
    }
    try {
      return json(await refresh(env, apiKey, body.refreshToken, body.organizationId));
    } catch (e) {
      return authFailed(e);
    }
  }

  if (parts[1] === "orgs" && parts.length === 2) {
    if (!apiKey) return notConfigured();
    const token = bearerFromRequest(request);
    const caller = token ? await verifyToken(env, token) : undefined;
    if (!caller) return json({ error: "invalid or missing bearer token" }, 401);
    if (request.method === "GET") {
      try {
        return json({ orgs: await listOrgs(apiKey, caller.userId) });
      } catch (e) {
        return authFailed(e);
      }
    }
    if (request.method === "POST") {
      const body = await bodyJson<{ name?: string }>(request);
      if (typeof body?.name !== "string") return json({ error: "missing name" }, 400);
      const trimmed = body.name.trim();
      if (trimmed.length === 0 || trimmed.length > 80) {
        return json({ error: "name must be 1-80 characters" }, 400);
      }
      try {
        return json(await createOrg(apiKey, caller.userId, trimmed));
      } catch (e) {
        return authFailed(e);
      }
    }
  }

  if (parts[1] === "cli" && parts[2] === "callback" && request.method === "GET") {
    return cliCallback(url);
  }

  return undefined;
};

// ---------------------------------------------------------------------------
// Headless sign-in callback
// ---------------------------------------------------------------------------

/** Query params land verbatim in the page — escape them. (WorkOS codes/states
 * are URL-safe tokens, but this URL accepts anything.) */
const escapeHtml = (s: string): string =>
  s
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");

const cliPage = (body: string): string => `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<meta name="robots" content="noindex" />
<title>Comet — sign in</title>
<style>
  body { margin: 0; min-height: 100vh; display: grid; place-items: center;
         background: #0a0a0a; color: #ededed;
         font: 15px/1.6 ui-sans-serif, system-ui, sans-serif; }
  main { max-width: 34rem; padding: 2rem; text-align: center; }
  h1 { font-size: 1.05rem; font-weight: 600; margin: 0 0 0.75rem; }
  p { color: #a1a1a1; margin: 0.25rem 0; }
  code#paste { display: block; margin: 1.25rem 0 0.75rem; padding: 0.9rem 1rem;
         background: #171717; border: 1px solid #2e2e2e; border-radius: 8px;
         font: 13px/1.5 ui-monospace, monospace; word-break: break-all;
         user-select: all; cursor: pointer; }
  button { margin-top: 0.25rem; padding: 0.45rem 1rem; border-radius: 8px;
         border: 1px solid #2e2e2e; background: #ededed; color: #0a0a0a;
         font: 500 13px ui-sans-serif, system-ui, sans-serif; cursor: pointer; }
</style>
</head>
<body><main>${body}</main></body>
</html>`;

const html = (body: string, status = 200): Response =>
  new Response(body, { status, headers: { "content-type": "text/html; charset=utf-8" } });

/**
 * The hosted OAuth callback for headless (paste-code) sign-in. Registered as a
 * WorkOS redirect URI; it does NOT exchange the code — it renders `state.code`
 * for the user to paste into the device that started the flow (`comet login`),
 * where the exchange runs so the tokens land on that machine. The state half
 * must match the pending sign-in there, so the paste is CSRF-checked at the
 * same point the loopback flow is.
 */
const cliCallback = (url: URL): Response => {
  const code = url.searchParams.get("code");
  const state = url.searchParams.get("state");
  const denied = url.searchParams.get("error");
  if (denied || !code || !state) {
    const detail = denied
      ? `Sign-in was not completed (${escapeHtml(denied)}).`
      : "This link is missing its sign-in code.";
    return html(
      cliPage(`<h1>Sign-in failed</h1><p>${detail}</p><p>Start again from your terminal.</p>`),
      400
    );
  }
  const paste = `${escapeHtml(state)}.${escapeHtml(code)}`;
  return html(
    cliPage(
      `<h1>Almost there</h1>
<p>Paste this code into the terminal that asked for it:</p>
<code id="paste">${paste}</code>
<button onclick="navigator.clipboard.writeText(document.getElementById('paste').textContent).then(()=>{this.textContent='Copied'})">Copy code</button>
<p style="margin-top:1rem;font-size:13px">This code expires in a few minutes and only works on the device that started sign-in.</p>`
    )
  );
};
