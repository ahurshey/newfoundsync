// Newfoundsync service worker — makes the web client an installable PWA.
//
// Strategy: NETWORK-FIRST for the app shell. On a LAN the server is fast, so we always
// serve fresh code when online (a cache-first SW would hand back stale app.js after a
// rebuild — exactly the "hard-refresh didn't help" trap). We only fall back to the cached
// shell when the network is unreachable, so an installed app still opens offline and shows
// its reconnecting state instead of the browser's error page.
//
// The audio/video stream and clock-sync travel over the WebSocket (/ws), which service
// workers never intercept — so streaming is completely unaffected by this file.

const CACHE = "nfs-shell-v2"; // bump on any shell change → activate() purges the stale precache
const SHELL = ["/", "/app.js", "/manifest.webmanifest", "/icon-128.png", "/icon-256.png", "/icon-512.png", "/icon-512-maskable.png", "/favicon.png"];

self.addEventListener("install", (e) => {
  e.waitUntil(caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting()));
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

self.addEventListener("fetch", (e) => {
  const req = e.request;
  const url = new URL(req.url);
  // Only the same-origin app shell. Cross-origin, non-GET, and the /ws upgrade are left to
  // the browser (WebSocket handshakes never reach the SW anyway).
  if (req.method !== "GET" || url.origin !== self.location.origin) return;

  e.respondWith(
    fetch(req)
      .then((res) => {
        // Cache a fresh copy of successful responses for offline use. (Same-origin is
        // already guaranteed above; no res.type filter — that could skip caching behind a
        // proxy/redirect and leave a stale offline shell.)
        if (res && res.ok) {
          const copy = res.clone();
          caches.open(CACHE).then((c) => c.put(req, copy)).catch(() => {});
        }
        return res;
      })
      .catch(() =>
        // Offline: serve the cached resource, or the cached shell for navigations.
        caches.match(req).then((hit) => hit || (req.mode === "navigate" ? caches.match("/") : Response.error()))
      )
  );
});
