/**
 * VaultRoom — the per-profile encrypted-sync control plane (RFC 0001 §6),
 * one Durable Object per `(orgId, userId)` (`vault1/{orgId}/{userId}`).
 *
 * It stores only public and encrypted material: the signed membership
 * history, HPKE keyring envelopes addressed to devices / the recovery
 * authority, per-object wrapped content keys, and short-lived enrollment
 * requests. There is no unlock endpoint, no administrative key, and no
 * plaintext keyring anywhere in this class. Every write is authenticated by
 * an Ed25519 signature checked against the membership head the room already
 * holds (or a self-signature for genesis / enrollment proofs); the account
 * bearer alone can read metadata but cannot alter trust.
 *
 * Routes (after the Worker's org check and user stamp):
 *   GET  /                         descriptor (404 when no vault exists)
 *   GET  /membership?after=N       signed policy records after sequence N
 *   POST /membership               append one signed record (CAS on parent)
 *   GET  /envelopes                recipient ids + epochs
 *   GET  /envelopes/:recipient     latest keyring envelope for a recipient
 *   PUT  /envelopes/:recipient     publish a keyring envelope (signed)
 *   GET  /objects/:object/keys     wrapped content keys for an object
 *   PUT  /objects/:object/keys     publish one wrapped key (first writer wins)
 *   GET  /enroll                   pending enrollment requests
 *   POST /enroll                   create a request (device-possession proof)
 *   GET  /enroll/:request          request status
 *   POST /enroll/:request/approve  mark approved (after membership + envelope)
 *   POST /enroll/:request/reject   reject / cancel
 */
import { AUTH_USER_HEADER, type Env } from "./env";
import {
  applyGenesis,
  applyMembership,
  bytesEqual,
  encodeBase64,
  epochRecipientId,
  hex,
  MAX_ENVELOPE_BYTES,
  MAX_POLICY_BYTES,
  MAX_RECORD_OVERHEAD,
  parseEnvelopeHeader,
  parsePolicyPayload,
  parseSignedRecord,
  pairingCode,
  POLICY_OBJECT_ID,
  RecipientKind,
  RecordError,
  RecordKind,
  DeviceStatus,
  unhex,
  verifyEd25519,
  verifyEnrollmentProof,
  type EnrollmentRequest,
  type MembershipHead,
  type SignedRecord
} from "./vault-records";

const ENROLLMENT_TTL_MS = 15 * 60 * 1000;
const MAX_PENDING_ENROLLMENTS = 16;
const MAX_MEMBERSHIP_PAGE = 256;
const MAX_OBJECT_KEYS_PER_OBJECT = 1024;
const HEX16 = /^[0-9a-f]{32}$/;

interface MembershipRow extends Record<string, SqlStorageValue> {
  seq: number;
  hash: ArrayBuffer;
  epoch: string;
  record: ArrayBuffer;
}

interface EnrollmentRow extends Record<string, SqlStorageValue> {
  request: ArrayBuffer;
  device: ArrayBuffer;
  signing: ArrayBuffer;
  encryption: ArrayBuffer;
  created_at: number;
  expires_at: number;
  status: string;
  membership_seq: number | null;
}

export class VaultRoom implements DurableObject {
  private readonly ctx: DurableObjectState;

  constructor(ctx: DurableObjectState, _env: Env) {
    this.ctx = ctx;
    ctx.storage.sql.exec(
      `CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
       CREATE TABLE IF NOT EXISTS membership (
         seq INTEGER PRIMARY KEY, hash BLOB NOT NULL, author BLOB NOT NULL,
         epoch TEXT NOT NULL, record BLOB NOT NULL, received_at INTEGER NOT NULL);
       CREATE TABLE IF NOT EXISTS envelopes (
         recipient BLOB PRIMARY KEY, kind INTEGER NOT NULL, epoch TEXT NOT NULL,
         author BLOB NOT NULL, record BLOB NOT NULL, updated_at INTEGER NOT NULL);
       CREATE TABLE IF NOT EXISTS object_keys (
         object BLOB NOT NULL, epoch TEXT NOT NULL, author BLOB NOT NULL,
         record BLOB NOT NULL, created_at INTEGER NOT NULL, PRIMARY KEY (object, epoch));
       CREATE TABLE IF NOT EXISTS enrollments (
         request BLOB PRIMARY KEY, device BLOB NOT NULL, signing BLOB NOT NULL,
         encryption BLOB NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
         status TEXT NOT NULL, membership_seq INTEGER);`
    );
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return json({ error: "unauthenticated" }, 401);
    const parts = url.pathname.split("/").filter(Boolean);
    const method = request.method;

    if (parts.length === 0) {
      return method === "GET" ? this.descriptor() : methodNotAllowed();
    }
    if (parts[0] === "membership" && parts.length === 1) {
      if (method === "GET") return this.membershipPage(url);
      if (method === "POST") return this.appendMembership(request);
      return methodNotAllowed();
    }
    if (parts[0] === "envelopes") {
      if (parts.length === 1 && method === "GET") return this.listEnvelopes();
      if (parts.length === 2 && HEX16.test(parts[1]!)) {
        const recipient = unhex(parts[1]!, 16)!;
        if (method === "GET") return this.getEnvelope(recipient);
        if (method === "PUT") return this.putEnvelope(recipient, request);
      }
      return methodNotAllowed();
    }
    if (parts[0] === "objects" && parts.length === 3 && HEX16.test(parts[1]!) && parts[2] === "keys") {
      const object = unhex(parts[1]!, 16)!;
      if (method === "GET") return this.getObjectKeys(object);
      if (method === "PUT") return this.putObjectKey(object, request);
      return methodNotAllowed();
    }
    if (parts[0] === "enroll") {
      this.expireEnrollments();
      if (parts.length === 1) {
        if (method === "GET") return this.listEnrollments();
        if (method === "POST") return this.createEnrollment(request);
        return methodNotAllowed();
      }
      if (HEX16.test(parts[1]!)) {
        const id = unhex(parts[1]!, 16)!;
        if (parts.length === 2 && method === "GET") return this.enrollmentStatus(id);
        if (parts.length === 3 && method === "POST" && parts[2] === "approve") {
          return this.approveEnrollment(id, request);
        }
        if (parts.length === 3 && method === "POST" && parts[2] === "reject") {
          return this.rejectEnrollment(id);
        }
      }
      return methodNotAllowed();
    }
    return json({ error: "not_found" }, 404);
  }

  // ── membership ────────────────────────────────────────────────────────────

  private get sql(): SqlStorage {
    return this.ctx.storage.sql;
  }

  private headRow(): MembershipRow | undefined {
    return [...this.sql.exec<MembershipRow>("SELECT seq, hash, epoch, record FROM membership ORDER BY seq DESC LIMIT 1")][0];
  }

  /** The verified head, rebuilt from the stored head record (records are
   * only stored after verification, so re-parsing is not re-trusting). */
  private head(): MembershipHead | undefined {
    const row = this.headRow();
    if (!row) return undefined;
    const record = parseSignedRecord(new Uint8Array(row.record), MAX_POLICY_BYTES);
    const payload = parsePolicyPayload(record.payload);
    return {
      sequence: BigInt(row.seq),
      hash: new Uint8Array(row.hash),
      epoch: BigInt(row.epoch),
      payload,
      vaultId: record.binding.vaultId,
      generation: record.binding.generation
    };
  }

  private headMatches(hash: Uint8Array): boolean {
    const current = this.headRow();
    return current !== undefined && bytesEqual(hash, new Uint8Array(current.hash));
  }

  private descriptor(): Response {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    return json({
      vaultId: hex(head.vaultId),
      generation: hex(head.generation),
      headSeq: Number(head.sequence),
      headHash: hex(head.hash),
      genesisHash: hex(this.genesisHash()),
      activeEpoch: head.epoch.toString(),
      profileHash: hex(head.payload.profileHash),
      activeDevices: head.payload.devices.filter((d) => d.status === DeviceStatus.active).length,
      devices: head.payload.devices.map((d) => ({
        deviceId: hex(d.deviceId),
        status: d.status === DeviceStatus.active ? "active" : "revoked"
      }))
    });
  }

  private membershipPage(url: URL): Response {
    const after = Number(url.searchParams.get("after") ?? "-1");
    if (!Number.isInteger(after) || after < -1) return json({ error: "bad_after" }, 400);
    const rows = [
      ...this.sql.exec<MembershipRow>(
        "SELECT seq, hash, epoch, record FROM membership WHERE seq > ? ORDER BY seq LIMIT ?",
        after,
        MAX_MEMBERSHIP_PAGE + 1
      )
    ];
    const head = this.headRow();
    const page = rows.slice(0, MAX_MEMBERSHIP_PAGE);
    return json({
      records: page.map((r) => encodeBase64(new Uint8Array(r.record))),
      hashes: page.map((r) => hex(new Uint8Array(r.hash))),
      truncated: rows.length > MAX_MEMBERSHIP_PAGE,
      headSeq: head ? head.seq : -1,
      headHash: head ? hex(new Uint8Array(head.hash)) : null
    });
  }

  private async appendMembership(request: Request): Promise<Response> {
    const body = await readBody(request, MAX_POLICY_BYTES + MAX_RECORD_OVERHEAD);
    if (!body) return json({ error: "too_large" }, 413);
    const head = this.head();
    const outcome = head ? await applyMembership(head, body) : await applyGenesis(body);
    if (!outcome.ok) {
      const status = outcome.error === "stale_parent" || outcome.error === "wrong_sequence" ? 409 : 400;
      return json(
        {
          error: outcome.error,
          headSeq: head ? Number(head.sequence) : -1,
          headHash: head ? hex(head.hash) : null
        },
        status
      );
    }
    const current = this.headRow();
    if (head ? !current || !bytesEqual(head.hash, new Uint8Array(current.hash)) : current !== undefined) {
      return json({ error: "stale_parent" }, 409);
    }
    // Genesis may only ever create; a second genesis on an existing vault
    // cannot reset trust (applyMembership already refuses op=genesis).
    this.sql.exec(
      "INSERT INTO membership (seq, hash, author, epoch, record, received_at) VALUES (?, ?, ?, ?, ?, ?)",
      Number(outcome.payload.sequence),
      buffer(outcome.hash),
      buffer(outcome.record.binding.authorId),
      outcome.epoch.toString(),
      buffer(body),
      Date.now()
    );
    return json({
      ok: true,
      headSeq: Number(outcome.payload.sequence),
      headHash: hex(outcome.hash),
      activeEpoch: outcome.epoch.toString()
    });
  }

  /** Parse + verify a key-envelope record signed by an active device of the
   * current head. Returns the record or an error response. */
  private async verifiedEnvelope(
    body: Uint8Array,
    expectedObject: Uint8Array
  ): Promise<{ record: SignedRecord; head: MembershipHead } | Response> {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    let record: SignedRecord;
    try {
      record = parseSignedRecord(body, MAX_ENVELOPE_BYTES);
    } catch (err) {
      return json({ error: err instanceof RecordError ? err.code : "malformed" }, 400);
    }
    const { binding } = record;
    if (
      binding.kind !== RecordKind.keyEnvelope ||
      !bytesEqual(binding.vaultId, head.vaultId) ||
      !bytesEqual(binding.generation, head.generation) ||
      !bytesEqual(binding.objectId, expectedObject)
    ) {
      return json({ error: "wrong_vault" }, 400);
    }
    if (!bytesEqual(binding.membershipHash, head.hash)) {
      return json({ error: "stale_parent", headSeq: Number(head.sequence), headHash: hex(head.hash) }, 409);
    }
    if (binding.epoch > head.epoch) return json({ error: "wrong_epoch" }, 400);
    const author = head.payload.devices.find((d) => bytesEqual(d.deviceId, binding.authorId));
    if (!author) return json({ error: "unknown_author" }, 403);
    if (author.status !== DeviceStatus.active) return json({ error: "revoked_author" }, 403);
    if (!(await verifyEd25519(author.signingKey, record.signingInput, record.signature))) {
      return json({ error: "invalid_signature" }, 403);
    }
    return { record, head };
  }

  // ── keyring envelopes ─────────────────────────────────────────────────────

  private listEnvelopes(): Response {
    const rows = [
      ...this.sql.exec<{ recipient: ArrayBuffer; kind: number; epoch: string; updated_at: number }>(
        "SELECT recipient, kind, epoch, updated_at FROM envelopes ORDER BY updated_at"
      )
    ];
    return json({
      envelopes: rows.map((r) => ({
        recipientId: hex(new Uint8Array(r.recipient)),
        recipientKind: r.kind,
        epoch: r.epoch,
        updatedAt: r.updated_at
      }))
    });
  }

  private getEnvelope(recipient: Uint8Array): Response {
    const row = [
      ...this.sql.exec<{ record: ArrayBuffer; epoch: string }>(
        "SELECT record, epoch FROM envelopes WHERE recipient = ?",
        buffer(recipient)
      )
    ][0];
    if (!row) return json({ error: "not_found" }, 404);
    return octets(new Uint8Array(row.record), { "x-zeron-vault-epoch": row.epoch });
  }

  private async putEnvelope(recipient: Uint8Array, request: Request): Promise<Response> {
    const body = await readBody(request, MAX_ENVELOPE_BYTES + MAX_RECORD_OVERHEAD);
    if (!body) return json({ error: "too_large" }, 413);
    const verified = await this.verifiedEnvelope(body, POLICY_OBJECT_ID);
    if (verified instanceof Response) return verified;
    const { record, head } = verified;
    if (!this.headMatches(head.hash)) return json({ error: "stale_parent" }, 409);
    let header;
    try {
      header = parseEnvelopeHeader(record.payload);
    } catch (err) {
      return json({ error: err instanceof RecordError ? err.code : "malformed" }, 400);
    }
    if (header.recipientKind === RecipientKind.epoch || !bytesEqual(header.recipientId, recipient)) {
      return json({ error: "wrong_recipient" }, 400);
    }
    // The recipient must be a device in the head (any status: a device may
    // receive its envelope in the same approval that adds it) or the
    // recovery authority; unknown recipients are not a storage service.
    if (header.recipientKind === RecipientKind.device) {
      if (!head.payload.devices.some((d) => bytesEqual(d.deviceId, recipient))) {
        return json({ error: "unknown_recipient" }, 400);
      }
    }
    const existing = [
      ...this.sql.exec<{ epoch: string }>("SELECT epoch FROM envelopes WHERE recipient = ?", buffer(recipient))
    ][0];
    if (existing && BigInt(existing.epoch) > record.binding.epoch) {
      return json({ error: "stale_epoch", epoch: existing.epoch }, 409);
    }
    this.sql.exec(
      `INSERT INTO envelopes (recipient, kind, epoch, author, record, updated_at) VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT(recipient) DO UPDATE SET kind = excluded.kind, epoch = excluded.epoch,
         author = excluded.author, record = excluded.record, updated_at = excluded.updated_at`,
      buffer(recipient),
      Number(header.recipientKind),
      record.binding.epoch.toString(),
      buffer(record.binding.authorId),
      buffer(body),
      Date.now()
    );
    return json({ ok: true, epoch: record.binding.epoch.toString() });
  }

  // ── object keys ───────────────────────────────────────────────────────────

  private getObjectKeys(object: Uint8Array): Response {
    const rows = [
      ...this.sql.exec<{ epoch: string; record: ArrayBuffer }>(
        "SELECT epoch, record FROM object_keys WHERE object = ? ORDER BY CAST(epoch AS INTEGER)",
        buffer(object)
      )
    ];
    return json({
      keys: rows.map((r) => ({ epoch: r.epoch, record: encodeBase64(new Uint8Array(r.record)) }))
    });
  }

  private async putObjectKey(object: Uint8Array, request: Request): Promise<Response> {
    const body = await readBody(request, MAX_ENVELOPE_BYTES + MAX_RECORD_OVERHEAD);
    if (!body) return json({ error: "too_large" }, 413);
    if (bytesEqual(object, POLICY_OBJECT_ID)) return json({ error: "wrong_object" }, 400);
    const verified = await this.verifiedEnvelope(body, object);
    if (verified instanceof Response) return verified;
    const { record, head } = verified;
    if (!this.headMatches(head.hash)) return json({ error: "stale_parent" }, 409);
    let header;
    try {
      header = parseEnvelopeHeader(record.payload);
    } catch (err) {
      return json({ error: err instanceof RecordError ? err.code : "malformed" }, 400);
    }
    if (
      header.recipientKind !== RecipientKind.epoch ||
      !bytesEqual(header.recipientId, epochRecipientId(record.binding.epoch))
    ) {
      return json({ error: "wrong_recipient" }, 400);
    }
    const epoch = record.binding.epoch.toString();
    const existing = [
      ...this.sql.exec<{ record: ArrayBuffer }>(
        "SELECT record FROM object_keys WHERE object = ? AND epoch = ?",
        buffer(object),
        epoch
      )
    ][0];
    if (existing) {
      const stored = new Uint8Array(existing.record);
      // First writer wins: a concurrent creator adopts the stored key so
      // every writer of this object/epoch seals under ONE root key.
      return json(
        { ok: bytesEqual(stored, body), conflict: !bytesEqual(stored, body), epoch, record: encodeBase64(stored) },
        bytesEqual(stored, body) ? 200 : 409
      );
    }
    const count = [
      ...this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM object_keys WHERE object = ?", buffer(object))
    ][0]!.n;
    if (count >= MAX_OBJECT_KEYS_PER_OBJECT) return json({ error: "too_many_keys" }, 429);
    this.sql.exec(
      "INSERT INTO object_keys (object, epoch, author, record, created_at) VALUES (?, ?, ?, ?, ?)",
      buffer(object),
      epoch,
      buffer(record.binding.authorId),
      buffer(body),
      Date.now()
    );
    return json({ ok: true, conflict: false, epoch, record: encodeBase64(body) });
  }

  // ── enrollment ────────────────────────────────────────────────────────────

  private expireEnrollments(): void {
    this.sql.exec(
      "UPDATE enrollments SET status = 'expired' WHERE status = 'pending' AND expires_at < ?",
      Date.now()
    );
    // Bound the table: keep the newest 64 non-pending rows.
    this.sql.exec(
      `DELETE FROM enrollments WHERE status != 'pending' AND request NOT IN
         (SELECT request FROM enrollments WHERE status != 'pending' ORDER BY created_at DESC LIMIT 64)`
    );
  }

  private genesisHash(): Uint8Array {
    const row = [...this.sql.exec<{ hash: ArrayBuffer }>("SELECT hash FROM membership WHERE seq = 0")][0];
    return row ? new Uint8Array(row.hash) : new Uint8Array(32);
  }

  private enrollmentJson(row: EnrollmentRow, vaultId: Uint8Array): Promise<Record<string, unknown>> {
    const request: EnrollmentRequest = {
      vaultId,
      requestId: new Uint8Array(row.request),
      deviceId: new Uint8Array(row.device),
      signingKey: new Uint8Array(row.signing),
      encryptionKey: new Uint8Array(row.encryption)
    };
    return pairingCode(request, this.genesisHash()).then((code) => ({
      requestId: hex(request.requestId),
      deviceId: hex(request.deviceId),
      signingKey: hex(request.signingKey),
      encryptionKey: hex(request.encryptionKey),
      // The server's copy of the code is a convenience for the approving
      // client's display only; each client recomputes it from the keys it
      // verifies, never from this field.
      pairingCode: code,
      createdAt: row.created_at,
      expiresAt: row.expires_at,
      status: row.status,
      membershipSeq: row.membership_seq
    }));
  }

  private async listEnrollments(): Promise<Response> {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    const rows = [
      ...this.sql.exec<EnrollmentRow>(
        "SELECT * FROM enrollments WHERE status = 'pending' ORDER BY created_at"
      )
    ];
    return json({ requests: await Promise.all(rows.map((r) => this.enrollmentJson(r, head.vaultId))) });
  }

  private async createEnrollment(request: Request): Promise<Response> {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    let body: { requestId?: string; deviceId?: string; signingKey?: string; encryptionKey?: string; proof?: string };
    try {
      body = (await request.json()) as typeof body;
    } catch {
      return json({ error: "bad_request" }, 400);
    }
    const requestId = unhex(body.requestId ?? "", 16);
    const deviceId = unhex(body.deviceId ?? "", 16);
    const signingKey = unhex(body.signingKey ?? "", 32);
    const encryptionKey = unhex(body.encryptionKey ?? "", 32);
    const proof = unhex(body.proof ?? "", 64);
    if (!requestId || !deviceId || !signingKey || !encryptionKey || !proof) {
      return json({ error: "bad_request" }, 400);
    }
    const enrollment: EnrollmentRequest = { vaultId: head.vaultId, requestId, deviceId, signingKey, encryptionKey };
    if (!(await verifyEnrollmentProof(enrollment, proof))) return json({ error: "invalid_proof" }, 403);
    if (head.payload.devices.some((d) => bytesEqual(d.deviceId, deviceId) || bytesEqual(d.signingKey, signingKey))) {
      return json({ error: "already_member" }, 409);
    }
    const pending = [
      ...this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM enrollments WHERE status = 'pending'")
    ][0]!.n;
    if (pending >= MAX_PENDING_ENROLLMENTS) return json({ error: "too_many_requests" }, 429);
    const existing = [
      ...this.sql.exec<EnrollmentRow>("SELECT * FROM enrollments WHERE request = ?", buffer(requestId))
    ][0];
    if (existing) {
      if (
        !bytesEqual(new Uint8Array(existing.device), deviceId) ||
        !bytesEqual(new Uint8Array(existing.signing), signingKey)
      ) {
        return json({ error: "conflict" }, 409);
      }
      return json(await this.enrollmentJson(existing, head.vaultId));
    }
    const now = Date.now();
    this.sql.exec(
      `INSERT INTO enrollments (request, device, signing, encryption, created_at, expires_at, status, membership_seq)
       VALUES (?, ?, ?, ?, ?, ?, 'pending', NULL)`,
      buffer(requestId),
      buffer(deviceId),
      buffer(signingKey),
      buffer(encryptionKey),
      now,
      now + ENROLLMENT_TTL_MS
    );
    const row = [...this.sql.exec<EnrollmentRow>("SELECT * FROM enrollments WHERE request = ?", buffer(requestId))][0]!;
    return json(await this.enrollmentJson(row, head.vaultId), 201);
  }

  private async enrollmentStatus(id: Uint8Array): Promise<Response> {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    const row = [...this.sql.exec<EnrollmentRow>("SELECT * FROM enrollments WHERE request = ?", buffer(id))][0];
    if (!row) return json({ error: "not_found" }, 404);
    return json(await this.enrollmentJson(row, head.vaultId));
  }

  /** Approval is only a bookkeeping mark: the approver must already have
   * appended the add-device membership record and published the device's
   * keyring envelope, both of which are independently signed. */
  private async approveEnrollment(id: Uint8Array, request: Request): Promise<Response> {
    const head = this.head();
    if (!head) return json({ error: "not_found" }, 404);
    const row = [...this.sql.exec<EnrollmentRow>("SELECT * FROM enrollments WHERE request = ?", buffer(id))][0];
    if (!row) return json({ error: "not_found" }, 404);
    if (row.status !== "pending") return json({ error: "not_pending", status: row.status }, 409);
    let body: { membershipSeq?: number };
    try {
      body = (await request.json()) as typeof body;
    } catch {
      return json({ error: "bad_request" }, 400);
    }
    const device = new Uint8Array(row.device);
    const member = head.payload.devices.find((d) => bytesEqual(d.deviceId, device));
    if (!member || member.status !== DeviceStatus.active) return json({ error: "not_member" }, 409);
    if (!bytesEqual(member.signingKey, new Uint8Array(row.signing))) return json({ error: "key_mismatch" }, 409);
    const envelope = [
      ...this.sql.exec<{ epoch: string }>("SELECT epoch FROM envelopes WHERE recipient = ?", buffer(device))
    ][0];
    if (!envelope) return json({ error: "envelope_missing" }, 409);
    const seq = Number.isInteger(body.membershipSeq) ? body.membershipSeq! : Number(head.sequence);
    this.sql.exec(
      "UPDATE enrollments SET status = 'approved', membership_seq = ? WHERE request = ?",
      seq,
      buffer(id)
    );
    return json({ ok: true, membershipSeq: seq });
  }

  private rejectEnrollment(id: Uint8Array): Response {
    const row = [...this.sql.exec<EnrollmentRow>("SELECT * FROM enrollments WHERE request = ?", buffer(id))][0];
    if (!row) return json({ error: "not_found" }, 404);
    if (row.status !== "pending") return json({ error: "not_pending", status: row.status }, 409);
    this.sql.exec("UPDATE enrollments SET status = 'rejected' WHERE request = ?", buffer(id));
    return json({ ok: true });
  }
}

// ── helpers ─────────────────────────────────────────────────────────────────

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });

const octets = (bytes: Uint8Array, extra: Record<string, string> = {}): Response =>
  new Response(buffer(bytes), {
    headers: {
      "content-type": "application/octet-stream",
      "content-length": String(bytes.byteLength),
      "cache-control": "private, no-store",
      ...extra
    }
  });

const methodNotAllowed = (): Response => json({ error: "not_found" }, 404);

const buffer = (bytes: Uint8Array): ArrayBuffer =>
  bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;

/** Read a bounded octet body; `undefined` when it exceeds `limit`. */
const readBody = async (request: Request, limit: number): Promise<Uint8Array | undefined> => {
  const declared = Number(request.headers.get("content-length") ?? "0");
  if (Number.isFinite(declared) && declared > limit) return undefined;
  const bytes = new Uint8Array(await request.arrayBuffer());
  if (bytes.byteLength > limit || bytes.byteLength === 0) return undefined;
  return bytes;
};
