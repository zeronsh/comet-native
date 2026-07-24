import { describe, expect, it } from "vitest";
import { pickLiveHost } from "./device-room";

// The bug this guards: a host whose uplink died silently leaves a socket the
// runtime still lists (no close event ever fires, and the supersede `close()`
// never completes either). Routing to the FIRST host socket pinned the room to
// that corpse — client frames vanished into it while the live host, which had
// reconnected and sat later in the list, received nothing. A non-empty host
// list also suppressed the `host_offline` bounce, so clients hung instead of
// failing fast.
describe("device-room host selection", () => {
  const NOW = 1_000_000_000_000;
  const fresh = NOW - 10_000; // pinged 10s ago
  const corpse = NOW - 10 * 60_000; // silent for 10 minutes

  it("prefers the live host over an older corpse listed first", () => {
    expect(
      pickLiveHost(
        [
          { ws: "corpse", lastSeenAt: corpse },
          { ws: "live", lastSeenAt: fresh }
        ],
        NOW
      )
    ).toBe("live");
  });

  it("still finds the live host when the corpse is listed last", () => {
    expect(
      pickLiveHost(
        [
          { ws: "live", lastSeenAt: fresh },
          { ws: "corpse", lastSeenAt: corpse }
        ],
        NOW
      )
    ).toBe("live");
  });

  it("picks the freshest of several live hosts", () => {
    expect(
      pickLiveHost(
        [
          { ws: "older", lastSeenAt: NOW - 40_000 },
          { ws: "newest", lastSeenAt: NOW - 1_000 },
          { ws: "middle", lastSeenAt: NOW - 20_000 }
        ],
        NOW
      )
    ).toBe("newest");
  });

  it("reports no host when every socket is stale — clients get host_offline", () => {
    expect(
      pickLiveHost(
        [
          { ws: "corpse-a", lastSeenAt: corpse },
          { ws: "corpse-b", lastSeenAt: NOW - 76_000 }
        ],
        NOW
      )
    ).toBeUndefined();
  });

  it("treats a socket attached before this deploy (no timestamps) as dead", () => {
    expect(pickLiveHost([{ ws: "legacy", lastSeenAt: 0 }], NOW)).toBeUndefined();
  });

  it("keeps a just-joined host that has not pinged yet", () => {
    expect(pickLiveHost([{ ws: "joining", lastSeenAt: NOW }], NOW)).toBe("joining");
  });

  it("has no host in an empty room", () => {
    expect(pickLiveHost([], NOW)).toBeUndefined();
  });
});
