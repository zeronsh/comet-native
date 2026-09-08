import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  applyGenesis,
  applyMembership,
  decodeBase64,
  DeviceStatus,
  hex,
  MAX_ENVELOPE_BYTES,
  parseEnvelopeHeader,
  parseSignedRecord,
  looksLikeSealedContent,
  looksLikeSealedField,
  encodeBase64,
  pairingCode,
  RecordKind,
  RecipientKind,
  recoveryAuthorityId,
  unhex,
  verifyEnrollmentProof,
  type MembershipHead
} from "./vault-records";

/** Rust-generated fixture (`crates/crypto/tests/vault_fixture.rs`): the same
 * bytes every verifier must accept, plus the hashes they must agree on. */
const fixture = JSON.parse(
  readFileSync(new URL("../../crates/crypto/tests/fixtures/vault.json", import.meta.url), "utf8")
) as {
  vaultId: string;
  generation: string;
  profileHash: string;
  recoverySigningKey: string;
  recoveryAuthorityId: string;
  deviceA: { id: string; signingKey: string };
  deviceB: { id: string };
  membership: string[];
  membershipHashes: string[];
  epochsAfter: number[];
  keyringEnvelopeB: string;
  objectId: string;
  objectKeyEnvelope: string;
  chatRecord: string;
  enrollment: {
    requestId: string;
    deviceId: string;
    signingKey: string;
    encryptionKey: string;
    proof: string;
    pairingCode: string;
  };
};

const records = fixture.membership.map((r) => decodeBase64(r)!);

const chain = async (): Promise<MembershipHead[]> => {
  const heads: MembershipHead[] = [];
  const genesis = await applyGenesis(records[0]!, unhex(fixture.profileHash)!);
  if (!genesis.ok) throw new Error(genesis.error);
  let head: MembershipHead = {
    sequence: 0n,
    hash: genesis.hash,
    epoch: 1n,
    payload: genesis.payload,
    vaultId: genesis.record.binding.vaultId,
    generation: genesis.record.binding.generation
  };
  heads.push(head);
  for (const record of records.slice(1)) {
    const next = await applyMembership(head, record);
    if (!next.ok) throw new Error(next.error);
    head = { ...head, sequence: next.payload.sequence, hash: next.hash, epoch: next.epoch, payload: next.payload };
    heads.push(head);
  }
  return heads;
};

describe("vault records (shared fixture)", () => {
  it("verifies the membership chain and agrees on every hash and epoch", async () => {
    const heads = await chain();
    expect(heads.map((h) => hex(h.hash))).toEqual(fixture.membershipHashes);
    expect(heads.map((h) => Number(h.epoch))).toEqual(fixture.epochsAfter);
    const final = heads[heads.length - 1]!;
    const b = final.payload.devices.find((d) => hex(d.deviceId) === fixture.deviceB.id);
    expect(b?.status).toBe(DeviceStatus.revoked);
    expect(hex(await recoveryAuthorityId(final.payload.recoverySigningKey))).toBe(
      fixture.recoveryAuthorityId
    );
    expect(hex(final.vaultId)).toBe(fixture.vaultId);
    expect(hex(final.generation)).toBe(fixture.generation);
  });

  it("rejects replays, forks, tampering, and the wrong profile", async () => {
    const heads = await chain();
    const [genesisHead, addedHead] = [heads[0]!, heads[1]!];
    // Replay on a moved head.
    expect((await applyMembership(addedHead, records[1]!)).ok).toBe(false);
    // Revoke record built on the add-head cannot follow genesis directly.
    expect((await applyMembership(genesisHead, records[2]!)).ok).toBe(false);
    // Any single-bit flip fails closed.
    for (const index of [0, 5, 40, 120, records[1]!.length - 1]) {
      const damaged = new Uint8Array(records[1]!);
      damaged[index] = damaged[index]! ^ 1;
      expect((await applyMembership(genesisHead, damaged)).ok).toBe(false);
    }
    const wrongProfile = await applyGenesis(records[0]!, new Uint8Array(32));
    expect(wrongProfile).toEqual({ ok: false, error: "wrong_profile" });
    // A non-genesis record is not a genesis.
    expect((await applyGenesis(records[1]!)).ok).toBe(false);
  });

  it("parses envelope headers and content wrappers without trusting them", () => {
    const keyring = parseSignedRecord(decodeBase64(fixture.keyringEnvelopeB)!, MAX_ENVELOPE_BYTES);
    expect(keyring.binding.kind).toBe(RecordKind.keyEnvelope);
    const header = parseEnvelopeHeader(keyring.payload);
    expect(header.recipientKind).toBe(RecipientKind.device);
    expect(hex(header.recipientId)).toBe(fixture.deviceB.id);
    const objectKey = parseSignedRecord(decodeBase64(fixture.objectKeyEnvelope)!, MAX_ENVELOPE_BYTES);
    expect(hex(objectKey.binding.objectId)).toBe(fixture.objectId);
    expect(parseEnvelopeHeader(objectKey.payload).recipientKind).toBe(RecipientKind.epoch);
    const chat = parseSignedRecord(decodeBase64(fixture.chatRecord)!, 2048);
    expect(chat.binding.kind).toBe(RecordKind.content);
    expect(hex(chat.binding.authorId)).toBe(fixture.deviceA.id);
    expect(() => parseSignedRecord(decodeBase64(fixture.chatRecord)!.subarray(1), 2048)).toThrow();
    // Framing check: content records pass; policy records, plaintext, and
    // truncated bytes do not.
    expect(looksLikeSealedContent(decodeBase64(fixture.chatRecord)!, 2048)).toBe(true);
    expect(looksLikeSealedContent(records[0]!, 65536)).toBe(false);
    expect(looksLikeSealedContent(new TextEncoder().encode("{\"title\":\"plain\"}"), 2048)).toBe(false);
    expect(looksLikeSealedContent(decodeBase64(fixture.chatRecord)!.subarray(0, 40), 2048)).toBe(false);
  });

  it("checks encrypted payload framing and purpose without claiming signature verification", () => {
    const bytes = decodeBase64(fixture.chatRecord)!;
    expect(looksLikeSealedContent(bytes, 2048, 1n)).toBe(true);
    expect(looksLikeSealedContent(bytes, 2048, 4n)).toBe(false);
    expect(looksLikeSealedField({ e1: fixture.chatRecord }, 2048)).toBe(false);
    expect(looksLikeSealedField("plain", 2048)).toBe(false);
    const payload = parseSignedRecord(bytes, 2048).payload;
    const offset = bytes.findIndex((_, index) => payload.every((byte, j) => bytes[index + j] === byte));
    expect(offset).toBeGreaterThan(0);
    const field = bytes.slice();
    field[offset + 6] = 4;
    expect(looksLikeSealedField({ e1: encodeBase64(field) }, 2048)).toBe(true);
    expect(looksLikeSealedField({ e1: encodeBase64(field), plain: "extra" }, 2048)).toBe(false);
    field[offset + 4] = 99;
    expect(looksLikeSealedContent(field, 2048)).toBe(false);
  });

  it("verifies enrollment proofs and derives the same pairing code", async () => {
    const request = {
      vaultId: unhex(fixture.vaultId, 16)!,
      requestId: unhex(fixture.enrollment.requestId, 16)!,
      deviceId: unhex(fixture.enrollment.deviceId, 16)!,
      signingKey: unhex(fixture.enrollment.signingKey, 32)!,
      encryptionKey: unhex(fixture.enrollment.encryptionKey, 32)!
    };
    const proof = unhex(fixture.enrollment.proof, 64)!;
    expect(await verifyEnrollmentProof(request, proof)).toBe(true);
    const genesis = unhex(fixture.membershipHashes[0]!, 32)!;
    expect(await pairingCode(request, genesis)).toBe(fixture.enrollment.pairingCode);
    const swapped = { ...request, encryptionKey: new Uint8Array(32).fill(9) };
    expect(await verifyEnrollmentProof(swapped, proof)).toBe(false);
    expect(await pairingCode(swapped, genesis)).not.toBe(fixture.enrollment.pairingCode);
    expect(await pairingCode(request, new Uint8Array(32))).not.toBe(fixture.enrollment.pairingCode);
    // The identity point is not an acceptable signing key.
    const identity = { ...request, signingKey: (() => { const b = new Uint8Array(32); b[0] = 1; return b; })() };
    expect(await verifyEnrollmentProof(identity, proof)).toBe(false);
  });
});
