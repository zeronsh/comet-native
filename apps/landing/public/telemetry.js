(() => {
  const optedOut = () => navigator.globalPrivacyControl === true || ["1", "yes"].includes(navigator.doNotTrack);
  if (
    !["https://zeron.sh", "https://comet.zeron.sh"].includes(location.origin) ||
    !["/", "/index.html"].includes(location.pathname) ||
    optedOut() || typeof fetch !== "function" ||
    typeof crypto === "undefined" || typeof crypto.randomUUID !== "function"
  ) return;

  const distinctId = crypto.randomUUID();
  let sessionId, sessionStartedAt;
  let referrer = "$direct", referringDomain = "$direct";
  try {
    const url = new URL(document.referrer);
    if (["https:", "http:"].includes(url.protocol)) {
      referrer = url.origin + "/";
      referringDomain = url.hostname;
    }
  } catch {}

  const capture = (event, properties = {}) => {
    if (optedOut()) return;
    try {
      const now = Date.now();
      if (!sessionId || now < sessionStartedAt || now - sessionStartedAt >= 24 * 60 * 60 * 1000) {
        const sessionTimestamp = now.toString(16).padStart(12, "0");
        sessionId = `${sessionTimestamp.slice(0, 8)}-${sessionTimestamp.slice(8)}-7${crypto.randomUUID().slice(15)}`;
        sessionStartedAt = now;
      }
      fetch("https://us.i.posthog.com/i/v0/e/?ip=0", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "omit",
        referrerPolicy: "no-referrer",
        keepalive: true,
        body: JSON.stringify({
          api_key: "phc_yAQTUdUM9vQMcHpGmMcNJom6vfrN3r3eo5avDSnt8S9R",
          event,
          timestamp: new Date(now).toISOString(),
          properties: {
            distinct_id: distinctId,
            $process_person_profile: false,
            $geoip_disable: true,
            $session_id: sessionId,
            $current_url: location.origin + location.pathname,
            $host: location.hostname,
            $pathname: location.pathname,
            $referrer: referrer,
            $referring_domain: referringDomain,
            ...properties,
          },
        }),
      }).catch(() => {});
    } catch {}
  };

  capture("$pageview");

  const placements = { "nav-download": "nav", "hero-download": "hero", "closing-download": "closing" };
  for (const [id, placement] of Object.entries(placements)) {
    const link = document.getElementById(id);
    if (!link) continue;
    const track = (event) => {
      if (event.defaultPrevented) return;
      let url;
      try { url = new URL(link.href); } catch { return; }
      const release = url.pathname.match(/^\/releases\/zeron-(\d+\.\d+\.\d+)-macos-arm64\.dmg$/);
      if (url.origin !== "https://zeron.sh" || !release) return;
      capture("download_clicked", {
        placement,
        version: release[1],
        platform: "macos",
        architecture: "arm64",
      });
    };
    link.addEventListener("click", track);
    link.addEventListener("auxclick", (event) => { if (event.button === 1) track(event); });
  }
})();
