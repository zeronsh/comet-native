import { AUTH_USER_HEADER, type Env } from "./env";

export async function vaultIsActive(lookup: () => Promise<Response>): Promise<boolean> {
  try {
    return (await lookup()).status !== 404;
  } catch {
    return true;
  }
}

export async function profileRequiresEncryption(
  env: Env,
  orgId: string | undefined,
  userId: string
): Promise<boolean> {
  if (!orgId) return env.AUTH_MODE !== "dev";
  return vaultIsActive(() => {
    const stub = env.VAULT_ROOMS.get(env.VAULT_ROOMS.idFromName(`vault1/${orgId}/${userId}`));
    return stub.fetch("https://vault/", { headers: { [AUTH_USER_HEADER]: userId } });
  });
}
