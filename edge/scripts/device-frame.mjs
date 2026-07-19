/** Mirror of src/device-room.ts frame encoding for smoke tests (plain JS). */

const writeUleb128 = (n) => {
  const out = [];
  do {
    let byte = n & 0x7f;
    n >>>= 7;
    if (n !== 0) byte |= 0x80;
    out.push(byte);
  } while (n !== 0);
  return out;
};

export const encodeDeviceFrame = (header, payload) => {
  const headerBytes = new TextEncoder().encode(JSON.stringify(header));
  const len = writeUleb128(headerBytes.length);
  const out = new Uint8Array(len.length + headerBytes.length + payload.length);
  out.set(len, 0);
  out.set(headerBytes, len.length);
  out.set(payload, len.length + headerBytes.length);
  return out;
};

export const decodeDeviceFrame = (bytes) => {
  let offset = 0;
  let len = 0;
  let shift = 0;
  for (;;) {
    const byte = bytes[offset++];
    len |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) break;
    shift += 7;
  }
  const header = JSON.parse(new TextDecoder().decode(bytes.subarray(offset, offset + len)));
  return { header, payload: bytes.subarray(offset + len) };
};
