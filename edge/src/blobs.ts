/**
 * Chunked blob storage over a DO's SQLite. Durable Object SQL caps individual
 * values at ~2MB; session snapshots and diff sidecars can exceed that, so
 * named blobs are stored as ordered chunk rows.
 */

const CHUNK_BYTES = 1_500_000;

export interface BlobStore {
  put(name: string, bytes: Uint8Array): void;
  get(name: string): Uint8Array | undefined;
  delete(name: string): void;
}

export const createBlobStore = (sql: SqlStorage): BlobStore => {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS blobs (name TEXT NOT NULL, idx INTEGER NOT NULL, bytes BLOB NOT NULL, PRIMARY KEY (name, idx))"
  );
  return {
    put(name, bytes) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
      for (let i = 0, idx = 0; i === 0 || i < bytes.length; i += CHUNK_BYTES, idx++) {
        const chunk = bytes.subarray(i, Math.min(i + CHUNK_BYTES, bytes.length));
        sql.exec("INSERT INTO blobs (name, idx, bytes) VALUES (?, ?, ?)", name, idx, chunk.buffer.slice(chunk.byteOffset, chunk.byteOffset + chunk.byteLength));
      }
    },
    get(name) {
      const rows = [...sql.exec("SELECT bytes FROM blobs WHERE name = ? ORDER BY idx", name)];
      if (rows.length === 0) return undefined;
      const parts = rows.map((r) => new Uint8Array(r.bytes as ArrayBuffer));
      const total = parts.reduce((a, p) => a + p.length, 0);
      const out = new Uint8Array(total);
      let off = 0;
      for (const p of parts) {
        out.set(p, off);
        off += p.length;
      }
      return out;
    },
    delete(name) {
      sql.exec("DELETE FROM blobs WHERE name = ?", name);
    }
  };
};

export const textEncoder = new TextEncoder();
export const textDecoder = new TextDecoder();

export const putJsonBlob = (store: BlobStore, name: string, value: unknown): void =>
  store.put(name, textEncoder.encode(JSON.stringify(value)));

export const getJsonBlob = <T>(store: BlobStore, name: string): T | undefined => {
  const bytes = store.get(name);
  if (!bytes) return undefined;
  try {
    return JSON.parse(textDecoder.decode(bytes)) as T;
  } catch {
    return undefined;
  }
};
