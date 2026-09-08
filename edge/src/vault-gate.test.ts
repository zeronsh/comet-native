import { describe, expect, it } from "vitest";
import { vaultIsActive } from "./vault-gate";

describe("legacy write fence", () => {
  it.each([200, 400, 401, 403, 429, 500, 503])("refuses plaintext on descriptor status %s", async (status) => {
    expect(await vaultIsActive(async () => new Response(null, { status }))).toBe(true);
  });

  it("only permits an explicit absent-vault response", async () => {
    expect(await vaultIsActive(async () => new Response(null, { status: 404 }))).toBe(false);
  });

  it("fails closed on a control-plane exception", async () => {
    expect(await vaultIsActive(async () => { throw new Error("unavailable"); })).toBe(true);
  });

  it("rechecks absence after activation", async () => {
    let status = 404;
    const lookup = async () => new Response(null, { status });
    expect(await vaultIsActive(lookup)).toBe(false);
    status = 200;
    expect(await vaultIsActive(lookup)).toBe(true);
  });
});
