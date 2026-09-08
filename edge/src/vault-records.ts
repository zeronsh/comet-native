/**
 * Vault control-plane record codec — the TypeScript twin of the fixed
 * deterministic-CBOR grammar in `crates/crypto/src/{record,policy,envelope}.rs`
 * (RFC 0001 §7.5, §5). The edge never holds a private key: it parses signed
 * wrappers, verifies Ed25519 signatures with WebCrypto against keys taken
 * from the membership head it already stores, and enforces the structural
 * transition rules so a stale, forked, or forged record cannot enter the
 * history. Clients re-verify everything independently.
 *
 * This is NOT a general CBOR decoder: it accepts exactly the shapes the Rust
 * side emits and rejects everything else (duplicates, reordering, indefinite
 * lengths, tags, non-shortest integers, trailing bytes).
 */

export const RECORD_DOMAIN = "zeron/signed-record/v1\0";
const MEMBERSHIP_DOMAIN = "zeron/membership/v1\0";
const RECOVERY_ID_DOMAIN = "zeron/recovery-id/v1\0";
const ENROLL_DOMAIN = "zeron/enroll/v1\0";
const PAIRING_DOMAIN = "zeron/pairing-code/v1\0";
export const MAX_RECORD_OVERHEAD = 256;
export const MAX_POLICY_BYTES = 64 * 1024;
export const MAX_ENVELOPE_BYTES = 16 + 1024 * 44 + 16 + 128;
export const MAX_DEVICES = 64;
export const POLICY_OBJECT_ID = new Uint8Array(16);

export const RecordKind = { policy: 1n, keyEnvelope: 2n, content: 3n } as const;
export const Operation = {
  genesis: 1n,
  addDevice: 2n,
  revokeDevice: 3n,
  rotateRecovery: 4n,
  recoveryTransition: 5n
} as const;
export const DeviceStatus = { active: 0n, revoked: 1n } as const;
export const RecipientKind = { device: 1n, recovery: 2n, epoch: 3n } as const;

export class RecordError extends Error {
  constructor(readonly code: string) {
    super(code);
  }
}

const textEncoder = new TextEncoder();

export interface RecordBinding {
  kind: bigint;
  vaultId: Uint8Array;
  generation: Uint8Array;
  epoch: bigint;
  objectId: Uint8Array;
  authorId: Uint8Array;
  membershipHash: Uint8Array;
}

export interface SignedRecord {
  binding: RecordBinding;
  revisionId: Uint8Array;
  payload: Uint8Array;
  signature: Uint8Array;
  /** Domain || deterministic map of fields 0..9 — what the signature covers. */
  signingInput: Uint8Array;
  encoded: Uint8Array;
}

export const bytesEqual = (a: Uint8Array, b: Uint8Array): boolean => {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
};

export const hex = (bytes: Uint8Array): string =>
  Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

export const unhex = (text: string, length?: number): Uint8Array | undefined => {
  if (!/^[0-9a-f]*$/i.test(text) || text.length % 2 !== 0) return undefined;
  if (length !== undefined && text.length !== length * 2) return undefined;
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(text.slice(i * 2, i * 2 + 2), 16);
  return out;
};

export const encodeBase64 = (bytes: Uint8Array): string => {
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000) {
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  }
  return btoa(bin);
};

export const decodeBase64 = (text: string): Uint8Array | undefined => {
  try {
    const bin = atob(text);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  } catch {
    return undefined;
  }
};

export const concat = (...parts: Uint8Array[]): Uint8Array => {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
};

const ascii = (text: string): Uint8Array => textEncoder.encode(text);

/** Cursor over the fixed grammar; every method throws `RecordError`. */
export class Reader {
  private offset = 0;
  constructor(private readonly bytes: Uint8Array) {}

  get position(): number {
    return this.offset;
  }

  get atEnd(): boolean {
    return this.offset >= this.bytes.length;
  }

  finish(): void {
    if (!this.atEnd) throw new RecordError("malformed");
  }

  private take(length: number): Uint8Array {
    if (length > this.bytes.length - this.offset) throw new RecordError("malformed");
    const out = this.bytes.subarray(this.offset, this.offset + length);
    this.offset += length;
    return out;
  }

  argument(major: number): bigint {
    const head = this.take(1)[0]!;
    if (head >> 5 !== major) throw new RecordError("malformed");
    const additional = head & 31;
    if (additional < 24) return BigInt(additional);
    let length: number;
    let minimum: bigint;
    switch (additional) {
      case 24:
        length = 1;
        minimum = 24n;
        break;
      case 25:
        length = 2;
        minimum = 0x100n;
        break;
      case 26:
        length = 4;
        minimum = 0x10000n;
        break;
      case 27:
        length = 8;
        minimum = 0x100000000n;
        break;
      default:
        throw new RecordError("malformed");
    }
    let value = 0n;
    for (const byte of this.take(length)) value = (value << 8n) | BigInt(byte);
    if (value < minimum) throw new RecordError("non_canonical");
    return value;
  }

  private key(expected: number): void {
    if (this.argument(0) !== BigInt(expected)) throw new RecordError("malformed");
  }

  uintField(key: number): bigint {
    this.key(key);
    return this.argument(0);
  }

  bytesField(key: number, limit: number): Uint8Array {
    this.key(key);
    const length = this.argument(2);
    if (length > BigInt(limit)) throw new RecordError("size_limit_exceeded");
    return this.take(Number(length));
  }

  fixedField(key: number, count: number): Uint8Array {
    const value = this.bytesField(key, count);
    if (value.length !== count) throw new RecordError("malformed");
    return value;
  }

  fixedBytes(count: number): Uint8Array {
    const length = this.argument(2);
    if (length !== BigInt(count)) throw new RecordError("malformed");
    return this.take(count);
  }
}

/** Parse a signed wrapper (no signature check). */
export const parseSignedRecord = (encoded: Uint8Array, maxPayloadBytes: number): SignedRecord => {
  if (encoded.length > maxPayloadBytes + MAX_RECORD_OVERHEAD) {
    throw new RecordError("size_limit_exceeded");
  }
  const reader = new Reader(encoded);
  if (reader.argument(5) !== 11n) throw new RecordError("malformed");
  if (reader.uintField(0) !== 1n) throw new RecordError("unsupported_version");
  const kind = reader.uintField(1);
  if (kind < 1n || kind > 3n) throw new RecordError("unsupported_kind");
  const vaultId = reader.fixedField(2, 16);
  const generation = reader.fixedField(3, 16);
  const epoch = reader.uintField(4);
  if (epoch === 0n) throw new RecordError("invalid_epoch");
  const objectId = reader.fixedField(5, 16);
  const authorId = reader.fixedField(6, 16);
  const revisionId = reader.fixedField(7, 16);
  const membershipHash = reader.fixedField(8, 32);
  const payload = reader.bytesField(9, maxPayloadBytes);
  const signedEnd = reader.position;
  const signature = reader.fixedField(10, 64);
  reader.finish();
  const signingInput = concat(
    ascii(RECORD_DOMAIN),
    new Uint8Array([0xaa]),
    encoded.subarray(1, signedEnd)
  );
  return {
    binding: { kind, vaultId, generation, epoch, objectId, authorId, membershipHash },
    revisionId,
    payload,
    signature,
    signingInput,
    encoded
  };
};

export interface DeviceEntry {
  deviceId: Uint8Array;
  signingKey: Uint8Array;
  encryptionKey: Uint8Array;
  status: bigint;
}

export interface PolicyPayload {
  sequence: bigint;
  parentHash: Uint8Array;
  profileHash: Uint8Array;
  epoch: bigint;
  operation: bigint;
  recoverySigningKey: Uint8Array;
  recoveryEncryptionKey: Uint8Array;
  devices: DeviceEntry[];
}

export const parsePolicyPayload = (bytes: Uint8Array): PolicyPayload => {
  const reader = new Reader(bytes);
  if (reader.argument(5) !== 9n) throw new RecordError("malformed");
  if (reader.uintField(0) !== 1n) throw new RecordError("unsupported_version");
  const sequence = reader.uintField(1);
  const parentHash = reader.fixedField(2, 32);
  const profileHash = reader.fixedField(3, 32);
  const epoch = reader.uintField(4);
  const operation = reader.uintField(5);
  if (operation < 1n || operation > 5n) throw new RecordError("unsupported_operation");
  const recoverySigningKey = reader.fixedField(6, 32);
  const recoveryEncryptionKey = reader.fixedField(7, 32);
  if (reader.argument(0) !== 8n) throw new RecordError("malformed");
  const count = reader.argument(4);
  if (count > BigInt(MAX_DEVICES)) throw new RecordError("too_many_devices");
  const devices: DeviceEntry[] = [];
  for (let i = 0n; i < count; i++) {
    if (reader.argument(4) !== 4n) throw new RecordError("malformed");
    const deviceId = reader.fixedBytes(16);
    const signingKey = reader.fixedBytes(32);
    const encryptionKey = reader.fixedBytes(32);
    const status = reader.argument(0);
    if (status !== 0n && status !== 1n) throw new RecordError("malformed");
    devices.push({ deviceId, signingKey, encryptionKey, status });
  }
  reader.finish();
  return {
    sequence,
    parentHash,
    profileHash,
    epoch,
    operation,
    recoverySigningKey,
    recoveryEncryptionKey,
    devices
  };
};

export interface EnvelopeHeader {
  recipientKind: bigint;
  recipientId: Uint8Array;
  encapsulation: Uint8Array;
  ciphertextLength: number;
}

export const parseEnvelopeHeader = (bytes: Uint8Array): EnvelopeHeader => {
  const reader = new Reader(bytes);
  if (reader.argument(5) !== 5n) throw new RecordError("unsupported_format");
  if (reader.uintField(0) !== 1n) throw new RecordError("unsupported_format");
  const recipientKind = reader.uintField(1);
  if (recipientKind < 1n || recipientKind > 3n) throw new RecordError("unsupported_format");
  const recipientId = reader.fixedField(2, 16);
  const encapsulation = reader.fixedField(3, 32);
  const ciphertext = reader.bytesField(4, MAX_ENVELOPE_BYTES);
  reader.finish();
  if (ciphertext.length < 16) throw new RecordError("unsupported_format");
  return { recipientKind, recipientId, encapsulation, ciphertextLength: ciphertext.length };
};

export const epochRecipientId = (epoch: bigint): Uint8Array => {
  const out = new Uint8Array(16);
  let value = epoch;
  for (let i = 15; i >= 8; i--) {
    out[i] = Number(value & 0xffn);
    value >>= 8n;
  }
  return out;
};

/** Protocol-framing check (RFC 0001 §12.2): does `bytes` parse as a signed
 * wrapper of kind = content? No signature or membership check — only the
 * relay's refusal to store bytes that are not even ciphertext-shaped in an
 * encrypted room. `maxPayloadBytes` bounds the parse. */
export const looksLikeSealedContent = (bytes: Uint8Array, maxPayloadBytes: number, expectedPurpose?: bigint): boolean => {
  try {
    const record = parseSignedRecord(bytes, maxPayloadBytes);
    if (record.binding.kind !== RecordKind.content) return false;
    const reader = new Reader(record.payload);
    if (reader.argument(5) !== 6n || reader.uintField(0) !== 1n || reader.uintField(1) !== 1n) return false;
    const purpose = reader.uintField(2);
    if (purpose < 1n || purpose > 8n || (expectedPurpose !== undefined && purpose !== expectedPurpose)) return false;
    reader.fixedField(3, 16);
    reader.fixedField(4, 32);
    const ciphertext = reader.bytesField(5, maxPayloadBytes);
    reader.finish();
    return ciphertext.length >= 16;
  } catch {
    return false;
  }
};

export const looksLikeSealedField = (value: unknown, maxPayloadBytes: number): boolean => {
  if (typeof value !== "object" || value === null || Array.isArray(value) || Object.keys(value).length !== 1) return false;
  if (!("e1" in value) || typeof value.e1 !== "string" || value.e1.length > (maxPayloadBytes + MAX_RECORD_OVERHEAD) * 2) return false;
  const bytes = decodeBase64(value.e1);
  return bytes !== undefined && looksLikeSealedContent(bytes, maxPayloadBytes, 4n);
};

// ── digests and signatures ──────────────────────────────────────────────────

export const sha256 = async (...parts: Uint8Array[]): Promise<Uint8Array> =>
  new Uint8Array(await crypto.subtle.digest("SHA-256", concat(...parts)));

export const membershipHash = (record: Uint8Array): Promise<Uint8Array> =>
  sha256(ascii(MEMBERSHIP_DOMAIN), record);

export const recoveryAuthorityId = async (signingKey: Uint8Array): Promise<Uint8Array> =>
  (await sha256(ascii(RECOVERY_ID_DOMAIN), signingKey)).subarray(0, 16);

const FIELD_MODULUS = (() => {
  const b = new Uint8Array(32).fill(0xff);
  b[0] = 0xed;
  b[31] = 0x7f;
  return b;
})();
const SMALL_ORDER_Y = [
  new Uint8Array(32),
  (() => {
    const b = new Uint8Array(32);
    b[0] = 1;
    return b;
  })(),
  (() => {
    const b = new Uint8Array(FIELD_MODULUS);
    b[0] = 0xec;
    return b;
  })(),
  unhex("26e8958fc2b227b045c3f489f2ef98f0d5dfac05d3c63339b13802886d53fc05")!,
  unhex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a")!
];
const SCALAR_ORDER = unhex("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010")!;

/** Little-endian a < b for 32-byte encodings. */
const lessThanLe = (a: Uint8Array, b: Uint8Array): boolean => {
  for (let i = 31; i >= 0; i--) {
    if (a[i]! !== b[i]!) return a[i]! < b[i]!;
  }
  return false;
};

/** RFC 0001 §7.6 point-encoding precheck (public data only). */
export const ed25519PointPrecheck = (encoded: Uint8Array): boolean => {
  if (encoded.length !== 32) return false;
  const y = new Uint8Array(encoded);
  y[31] = y[31]! & 0x7f;
  if (!lessThanLe(y, FIELD_MODULUS)) return false;
  return !SMALL_ORDER_Y.some((small) => bytesEqual(small, y));
};

export const ed25519ScalarPrecheck = (encoded: Uint8Array): boolean =>
  encoded.length === 32 && lessThanLe(encoded, SCALAR_ORDER);

export const verifyEd25519 = async (
  publicKey: Uint8Array,
  message: Uint8Array,
  signature: Uint8Array
): Promise<boolean> => {
  if (
    publicKey.length !== 32 ||
    signature.length !== 64 ||
    !ed25519PointPrecheck(publicKey) ||
    !ed25519PointPrecheck(signature.subarray(0, 32)) ||
    !ed25519ScalarPrecheck(signature.subarray(32))
  ) {
    return false;
  }
  try {
    const key = await crypto.subtle.importKey("raw", publicKey, { name: "Ed25519" }, false, [
      "verify"
    ]);
    return await crypto.subtle.verify({ name: "Ed25519" }, key, signature, message);
  } catch {
    return false;
  }
};

// ── membership transitions (mirror of policy.rs MembershipState) ────────────

export interface MembershipHead {
  sequence: bigint;
  hash: Uint8Array;
  epoch: bigint;
  payload: PolicyPayload;
  vaultId: Uint8Array;
  generation: Uint8Array;
}

export type ApplyOutcome =
  | { ok: true; record: SignedRecord; payload: PolicyPayload; hash: Uint8Array; epoch: bigint }
  | { ok: false; error: string };

const validDeviceEntries = (devices: DeviceEntry[]): boolean => {
  if (devices.length === 0 || devices.length > MAX_DEVICES) return false;
  for (let i = 0; i < devices.length; i++) {
    const device = devices[i]!;
    if (
      !ed25519PointPrecheck(device.signingKey) ||
      device.encryptionKey.every((b) => b === 0) ||
      bytesEqual(device.deviceId, POLICY_OBJECT_ID)
    ) {
      return false;
    }
    for (let j = 0; j < i; j++) {
      const other = devices[j]!;
      if (
        bytesEqual(other.deviceId, device.deviceId) ||
        bytesEqual(other.signingKey, device.signingKey) ||
        bytesEqual(other.encryptionKey, device.encryptionKey)
      ) {
        return false;
      }
    }
  }
  return true;
};

const validRecoveryKeys = (payload: PolicyPayload): boolean =>
  ed25519PointPrecheck(payload.recoverySigningKey) &&
  !payload.recoveryEncryptionKey.every((b) => b === 0) &&
  !payload.devices.some(
    (d) =>
      bytesEqual(d.signingKey, payload.recoverySigningKey) ||
      bytesEqual(d.encryptionKey, payload.recoveryEncryptionKey)
  );

const sameDevice = (a: DeviceEntry, b: DeviceEntry): boolean =>
  bytesEqual(a.deviceId, b.deviceId) &&
  bytesEqual(a.signingKey, b.signingKey) &&
  bytesEqual(a.encryptionKey, b.encryptionKey);

const sameEntry = (a: DeviceEntry, b: DeviceEntry): boolean =>
  sameDevice(a, b) && a.status === b.status;

/** Genesis: self-signed by the single listed device, sequence 0, epoch 1. */
export const applyGenesis = async (
  encoded: Uint8Array,
  expectedProfileHash?: Uint8Array
): Promise<ApplyOutcome> => {
  let record: SignedRecord;
  let payload: PolicyPayload;
  try {
    record = parseSignedRecord(encoded, MAX_POLICY_BYTES);
    payload = parsePolicyPayload(record.payload);
  } catch (err) {
    return { ok: false, error: err instanceof RecordError ? err.code : "malformed" };
  }
  const { binding } = record;
  if (binding.kind !== RecordKind.policy || !bytesEqual(binding.objectId, POLICY_OBJECT_ID)) {
    return { ok: false, error: "wrong_kind" };
  }
  if (payload.operation !== Operation.genesis) return { ok: false, error: "invalid_transition" };
  if (payload.sequence !== 0n || !payload.parentHash.every((b) => b === 0)) {
    return { ok: false, error: "wrong_sequence" };
  }
  if (expectedProfileHash && !bytesEqual(payload.profileHash, expectedProfileHash)) {
    return { ok: false, error: "wrong_profile" };
  }
  if (payload.epoch !== 1n || binding.epoch !== 1n) return { ok: false, error: "wrong_epoch" };
  if (!binding.membershipHash.every((b) => b === 0)) return { ok: false, error: "wrong_parent" };
  if (!validDeviceEntries(payload.devices) || payload.devices.length !== 1) {
    return { ok: false, error: "invalid_device_set" };
  }
  if (!validRecoveryKeys(payload)) return { ok: false, error: "invalid_recovery_keys" };
  const device = payload.devices[0]!;
  if (device.status !== DeviceStatus.active || !bytesEqual(binding.authorId, device.deviceId)) {
    return { ok: false, error: "invalid_device_set" };
  }
  if (!(await verifyEd25519(device.signingKey, record.signingInput, record.signature))) {
    return { ok: false, error: "invalid_signature" };
  }
  return { ok: true, record, payload, hash: await membershipHash(encoded), epoch: 1n };
};

/** Apply the next record on top of a verified head. */
export const applyMembership = async (
  head: MembershipHead,
  encoded: Uint8Array
): Promise<ApplyOutcome> => {
  let record: SignedRecord;
  let payload: PolicyPayload;
  try {
    record = parseSignedRecord(encoded, MAX_POLICY_BYTES);
    payload = parsePolicyPayload(record.payload);
  } catch (err) {
    return { ok: false, error: err instanceof RecordError ? err.code : "malformed" };
  }
  const { binding } = record;
  if (
    binding.kind !== RecordKind.policy ||
    !bytesEqual(binding.vaultId, head.vaultId) ||
    !bytesEqual(binding.generation, head.generation) ||
    !bytesEqual(binding.objectId, POLICY_OBJECT_ID)
  ) {
    return { ok: false, error: "wrong_vault" };
  }
  if (!bytesEqual(binding.membershipHash, head.hash) || !bytesEqual(payload.parentHash, head.hash)) {
    return { ok: false, error: "stale_parent" };
  }
  if (payload.sequence !== head.sequence + 1n) return { ok: false, error: "wrong_sequence" };
  if (!bytesEqual(payload.profileHash, head.payload.profileHash)) {
    return { ok: false, error: "wrong_profile" };
  }
  if (!validDeviceEntries(payload.devices)) return { ok: false, error: "invalid_device_set" };
  if (!validRecoveryKeys(payload)) return { ok: false, error: "invalid_recovery_keys" };

  let signingKey: Uint8Array;
  if (payload.operation === Operation.genesis) return { ok: false, error: "invalid_transition" };
  if (payload.operation === Operation.recoveryTransition) {
    const authority = await recoveryAuthorityId(head.payload.recoverySigningKey);
    if (!bytesEqual(binding.authorId, authority)) return { ok: false, error: "unknown_author" };
    signingKey = head.payload.recoverySigningKey;
  } else {
    const author = head.payload.devices.find((d) => bytesEqual(d.deviceId, binding.authorId));
    if (!author) return { ok: false, error: "unknown_author" };
    if (author.status !== DeviceStatus.active) return { ok: false, error: "revoked_author" };
    signingKey = author.signingKey;
  }
  const expectedEpoch = payload.operation === Operation.addDevice ? head.epoch : head.epoch + 1n;
  if (payload.epoch !== expectedEpoch || binding.epoch !== expectedEpoch) {
    return { ok: false, error: "wrong_epoch" };
  }
  const transition = checkTransition(head.payload, payload);
  if (transition) return { ok: false, error: transition };
  if (!(await verifyEd25519(signingKey, record.signingInput, record.signature))) {
    return { ok: false, error: "invalid_signature" };
  }
  return { ok: true, record, payload, hash: await membershipHash(encoded), epoch: payload.epoch };
};

const checkTransition = (previous: PolicyPayload, next: PolicyPayload): string | undefined => {
  const recoveryUnchanged =
    bytesEqual(next.recoverySigningKey, previous.recoverySigningKey) &&
    bytesEqual(next.recoveryEncryptionKey, previous.recoveryEncryptionKey);
  const recoveryReplaced =
    !bytesEqual(next.recoverySigningKey, previous.recoverySigningKey) &&
    !bytesEqual(next.recoveryEncryptionKey, previous.recoveryEncryptionKey);
  const prefixOk = (accept: (a: DeviceEntry, b: DeviceEntry) => boolean): boolean =>
    next.devices.length >= previous.devices.length &&
    previous.devices.every((p, i) => accept(p, next.devices[i]!));
  const added = next.devices.slice(previous.devices.length);
  switch (next.operation) {
    case Operation.addDevice:
      if (!recoveryUnchanged) return "invalid_recovery_keys";
      if (!prefixOk(sameEntry)) return "invalid_device_set";
      if (added.length !== 1 || added[0]!.status !== DeviceStatus.active) return "invalid_device_set";
      return undefined;
    case Operation.revokeDevice: {
      if (!recoveryUnchanged) return "invalid_recovery_keys";
      if (next.devices.length !== previous.devices.length) return "invalid_device_set";
      let revoked = 0;
      const ok = prefixOk((p, n) => {
        if (sameEntry(p, n)) return true;
        if (sameDevice(p, n) && p.status === DeviceStatus.active && n.status === DeviceStatus.revoked) {
          revoked++;
          return true;
        }
        return false;
      });
      return ok && revoked === 1 ? undefined : "invalid_device_set";
    }
    case Operation.rotateRecovery:
      if (!recoveryReplaced) return "invalid_recovery_keys";
      if (
        next.devices.length !== previous.devices.length ||
        !previous.devices.every((p, i) => sameEntry(p, next.devices[i]!))
      ) {
        return "invalid_device_set";
      }
      return undefined;
    case Operation.recoveryTransition:
      if (!(recoveryUnchanged || recoveryReplaced)) return "invalid_recovery_keys";
      if (
        !prefixOk((p, n) => sameEntry(p, n) || (sameDevice(p, n) && n.status === DeviceStatus.revoked))
      ) {
        return "invalid_device_set";
      }
      if (added.length !== 1 || added[0]!.status !== DeviceStatus.active) return "invalid_device_set";
      return undefined;
    default:
      return "unsupported_operation";
  }
};

// ── enrollment ──────────────────────────────────────────────────────────────

export interface EnrollmentRequest {
  vaultId: Uint8Array;
  requestId: Uint8Array;
  deviceId: Uint8Array;
  signingKey: Uint8Array;
  encryptionKey: Uint8Array;
}

const enrollmentBody = (request: EnrollmentRequest): Uint8Array =>
  concat(
    request.vaultId,
    request.requestId,
    request.deviceId,
    request.signingKey,
    request.encryptionKey
  );

export const verifyEnrollmentProof = async (
  request: EnrollmentRequest,
  proof: Uint8Array
): Promise<boolean> => {
  if (
    !ed25519PointPrecheck(request.signingKey) ||
    request.encryptionKey.every((b) => b === 0) ||
    bytesEqual(request.deviceId, POLICY_OBJECT_ID)
  ) {
    return false;
  }
  return verifyEd25519(
    request.signingKey,
    concat(ascii(ENROLL_DOMAIN), enrollmentBody(request)),
    proof
  );
};

/** "NNNN-NNNN" comparison code — same derivation as `policy.rs`. The
 * genesis hash is part of the input so a relay that presents the pending
 * device with a substitute vault produces a code the approver will not see. */
export const pairingCode = async (request: EnrollmentRequest, genesisHash: Uint8Array): Promise<string> => {
  const digest = await sha256(ascii(PAIRING_DOMAIN), enrollmentBody(request), genesisHash);
  const value =
    (((digest[0]! << 24) | (digest[1]! << 16) | (digest[2]! << 8) | digest[3]!) >>> 0) % 100_000_000;
  return `${String(Math.floor(value / 10_000)).padStart(4, "0")}-${String(value % 10_000).padStart(4, "0")}`;
};
