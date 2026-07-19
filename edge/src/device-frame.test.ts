import { describe, expect, it } from "vitest";
import { decodeDeviceFrame, encodeDeviceFrame } from "./device-room";

describe("device frame codec", () => {
  it("round-trips header + payload", () => {
    const payload = new Uint8Array([1, 2, 3, 250, 255]);
    const frame = encodeDeviceFrame({ s: "term-42", k: "term", to: "conn-9" }, payload);
    const decoded = decodeDeviceFrame(frame);
    expect(decoded.header).toEqual({ s: "term-42", k: "term", to: "conn-9" });
    expect([...decoded.payload]).toEqual([...payload]);
  });

  it("handles empty payloads and long headers", () => {
    const header = { s: "x".repeat(200), k: "rpc", from: "conn-1" };
    const decoded = decodeDeviceFrame(encodeDeviceFrame(header, new Uint8Array()));
    expect(decoded.header).toEqual(header);
    expect(decoded.payload.length).toBe(0);
  });
});
