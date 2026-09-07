import assert from "node:assert/strict";
import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import test from "node:test";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("./public/telemetry.js", import.meta.url), "utf8");
const html = readFileSync(new URL("./public/index.html", import.meta.url), "utf8");
const placements = { "nav-download": "nav", "hero-download": "hero", "closing-download": "closing" };

function load({ url = "https://zeron.sh/", referrer = "", navigator = {}, transport, clock = Date } = {}) {
  const requests = [];
  const links = Object.fromEntries(Object.keys(placements).map((id) => {
    const href = html.match(new RegExp(`id="${id}" href="([^"]+)"`))[1];
    const listeners = {};
    return [id, {
      href,
      addEventListener: (type, listener) => { listeners[type] = listener; },
      activate(type = "click", button = 0) {
        listeners[type]?.({ button, defaultPrevented: false });
      },
    }];
  }));
  const document = { referrer, getElementById: (id) => links[id] };
  const context = {
    location: new URL(url),
    navigator,
    document,
    crypto: { randomUUID },
    Date: clock,
    URL,
    fetch: (endpoint, options) => {
      requests.push({ endpoint, ...options, payload: JSON.parse(options.body) });
      return transport ? transport() : Promise.resolve({ ok: true });
    },
  };
  for (const property of ["localStorage", "sessionStorage"]) {
    Object.defineProperty(context, property, { get() { throw new Error(`Accessed ${property}`); } });
  }
  Object.defineProperty(document, "cookie", {
    get() { throw new Error("Read cookies"); },
    set() { throw new Error("Wrote cookies"); },
  });
  runInNewContext(source, context);
  return { requests, links, navigator };
}

test("HTML loads the tracker once without blocking parsing", () => {
  assert.equal(html.match(/<script defer src="\/telemetry\.js"><\/script>/g)?.length, 1);
});

test("captures one anonymous pageview using the US ingestion endpoint", () => {
  const { requests } = load();
  assert.equal(requests.length, 1);
  const request = requests[0];
  assert.equal(request.endpoint, "https://us.i.posthog.com/i/v0/e/?ip=0");
  assert.equal(request.method, "POST");
  assert.equal(request.headers["Content-Type"], "application/json");
  assert.equal(request.keepalive, true);
  assert.equal(request.credentials, "omit");
  assert.equal(request.referrerPolicy, "no-referrer");
  assert.match(request.payload.api_key, /^phc_[A-Za-z0-9]+$/);
  assert.equal(request.payload.event, "$pageview");
  assert.match(request.payload.properties.distinct_id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(request.payload.properties.$process_person_profile, false);
  assert.equal(request.payload.properties.$geoip_disable, true);
  assert.equal(request.payload.properties.$current_url, "https://zeron.sh/");
  assert.equal(request.payload.properties.$referring_domain, "$direct");
});

for (const now of [0x000123456789, Date.UTC(2026, 8, 4)]) {
  test(`uses a timestamped UUIDv7 session ID at ${now}`, () => {
    const clock = class extends Date {
      constructor() { super(now); }
      static now() { return now; }
    };
    const { requests, links } = load({ clock });
    const sessionId = requests[0].payload.properties.$session_id;
    assert.match(sessionId, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    const timestamp = Number.parseInt(sessionId.slice(0, 13).replace("-", ""), 16);
    assert.equal(timestamp, now);
    links["hero-download"].activate();
    for (const { payload } of requests) {
      assert.equal(payload.properties.$session_id, sessionId);
      assert.ok(timestamp <= Date.parse(payload.timestamp));
      assert.ok(Date.parse(payload.timestamp) < timestamp + 24 * 60 * 60 * 1000);
    }
    assert.notEqual(load({ clock }).requests[0].payload.properties.$session_id, sessionId);
  });
}

for (const [elapsed, rotates] of [[86400000 - 1, false], [86400000, true], [86400000 + 1, true], [3 * 86400000, true], [-1, true]]) {
  test(`maintains valid sessions after a clock change of ${elapsed} ms`, () => {
    let now = Date.UTC(2026, 8, 4);
    const clock = class extends Date {
      constructor(value = now) { super(value); }
      static now() { return now; }
    };
    const { requests, links } = load({ clock });
    const first = requests[0].payload;
    now += elapsed;
    links["hero-download"].activate();
    assert.equal(requests.length, 2);
    const sessionId = requests[1].payload.properties.$session_id;
    if (rotates) assert.notEqual(sessionId, first.properties.$session_id);
    else assert.equal(sessionId, first.properties.$session_id);
    if (rotates) {
      assert.equal(Number.parseInt(sessionId.slice(0, 13).replace("-", ""), 16), now);
    }
    links["hero-download"].activate();
    assert.equal(requests[2].payload.properties.$session_id, sessionId);
    now += 86400000;
    links["hero-download"].activate();
    assert.equal(requests.length, 4);
    assert.notEqual(requests[3].payload.properties.$session_id, sessionId);
    assert.equal(Date.parse(requests[3].payload.timestamp), now);
    for (const [index, { payload }] of requests.entries()) {
      const id = payload.properties.$session_id;
      assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
      const startedAt = Number.parseInt(id.slice(0, 13).replace("-", ""), 16);
      const eventTime = Date.parse(payload.timestamp);
      assert.ok(startedAt <= eventTime);
      assert.ok(eventTime < startedAt + 86400000);
      assert.equal(payload.properties.distinct_id, first.properties.distinct_id);
      assert.equal(payload.event, index === 0 ? "$pageview" : "download_clicked");
    }
  });
}

test("drops query strings, fragments, and referrer paths and credentials", () => {
  const { requests } = load({
    url: "https://zeron.sh/?email=private%40example.invalid#secret",
    referrer: "https://user:password@search.example.invalid/private?token=secret#fragment",
  });
  const properties = requests[0].payload.properties;
  assert.equal(properties.$current_url, "https://zeron.sh/");
  assert.equal(properties.$referrer, "https://search.example.invalid/");
  assert.equal(properties.$referring_domain, "search.example.invalid");
  assert.doesNotMatch(requests[0].body, /private|secret|password|user:|fragment/);
});

for (const referrer of ["not a URL", "about:blank", "file:///private/secret"]) {
  test(`ignores invalid or non-web referrer: ${referrer}`, () => {
    const { requests } = load({ referrer });
    assert.equal(requests[0].payload.properties.$referring_domain, "$direct");
    assert.equal(requests[0].payload.properties.$referrer, "$direct");
  });
}

for (const [id, placement] of Object.entries(placements)) {
  test(`tracks ${placement} download clicks with the current release version`, () => {
    const { requests, links } = load();
    links[id].href = "https://zeron.sh/releases/zeron-1.2.3-macos-arm64.dmg?private=secret#fragment";
    links[id].activate();
    assert.equal(requests.length, 2);
    const { payload } = requests[1];
    assert.equal(payload.event, "download_clicked");
    assert.equal(payload.properties.distinct_id, requests[0].payload.properties.distinct_id);
    assert.equal(payload.properties.$session_id, requests[0].payload.properties.$session_id);
    assert.equal(payload.properties.placement, placement);
    assert.equal(payload.properties.version, "1.2.3");
    assert.equal(payload.properties.platform, "macos");
    assert.equal(payload.properties.architecture, "arm64");
    assert.doesNotMatch(requests[1].body, /private|secret|fragment/);
  });
}

test("tracks the pinned fallback download when release lookup has not completed", () => {
  const { requests, links } = load();
  links["hero-download"].activate();
  assert.equal(requests.length, 2);
  assert.match(requests[1].payload.properties.version, /^\d+\.\d+\.\d+$/);
});

test("counts middle clicks but ignores right clicks", () => {
  const { requests, links } = load();
  links["hero-download"].activate("auxclick", 1);
  links["hero-download"].activate("auxclick", 2);
  assert.equal(requests.length, 2);
});

for (const href of ["https://example.invalid/releases/zeron-1.2.3-macos-arm64.dmg", "https://zeron.sh/private", "invalid"]) {
  test(`does not report unexpected download targets: ${href}`, () => {
    const { requests, links } = load();
    links["hero-download"].href = href;
    links["hero-download"].activate();
    assert.equal(requests.length, 1);
  });
}

for (const url of ["http://localhost:8000/", "https://preview.workers.dev/", "http://zeron.sh/", "https://zeron.sh/private", "https://zeron.sh:8000/"]) {
  test(`does not track development or unexpected URLs: ${url}`, () => {
    const { requests, links } = load({ url });
    links["hero-download"].activate();
    assert.equal(requests.length, 0);
  });
}

test("tracks the legacy production hostname", () => {
  assert.equal(load({ url: "https://comet.zeron.sh/" }).requests.length, 1);
});

for (const navigator of [{ doNotTrack: "1" }, { doNotTrack: "yes" }, { globalPrivacyControl: true }]) {
  test(`respects browser privacy preference: ${JSON.stringify(navigator)}`, () => {
    const { requests, links } = load({ navigator });
    links["hero-download"].activate();
    assert.equal(requests.length, 0);
  });
}

test("respects privacy preferences enabled after page load", () => {
  const { requests, links, navigator } = load();
  navigator.globalPrivacyControl = true;
  links["hero-download"].activate();
  assert.equal(requests.length, 1);
});

test("does not persist visitor identifiers across page loads", () => {
  assert.notEqual(load().requests[0].payload.properties.distinct_id, load().requests[0].payload.properties.distinct_id);
});

test("network rejection does not break downloads or create an unhandled rejection", async () => {
  const { requests, links } = load({ transport: () => Promise.reject(new Error("Blocked")) });
  links["hero-download"].activate();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(requests.length, 2);
});

test("synchronous transport failures do not break download handlers", () => {
  const { links } = load({ transport: () => { throw new Error("Unavailable"); } });
  assert.doesNotThrow(() => links["hero-download"].activate());
});
