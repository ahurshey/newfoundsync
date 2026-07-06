// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

// Newfoundsync service worker — makes the web client an installable PWA.
//
// Strategy (deliberately conservative after a nasty cache-skew bug):
//  - The SHELL (navigations, "/", "/app.js", and anything not a known static asset) is served
//    NETWORK-ONLY, with a short timeout, and is NEVER answered from cache. Serving a stale cached
//    app.js next to a fresh index.html is exactly the cross-build "half-broken, a reload won't fix
//    it" skew. On network failure we return a network error so the browser retries — and
//    index.html's <head> watchdog heals a genuinely stale shell (it does not depend on this SW).
//  - Only build-agnostic STATIC assets (icons + manifest) are cached, for installability and
//    offline icons; those use stale-while-revalidate. The cache bucket is build-stamped so
//    activate() purges older buckets.
//
// The audio/video stream and clock-sync travel over the WebSocket (/ws), which service workers
// never intercept — streaming is unaffected by this file.

const CACHE = "nfs-assets-" + "__NFS_BUILD__"; // build-stamped by the server; activate() purges the rest
const STATIC = [
  "/manifest.webmanifest",
  "/icon-128.png",
  "/icon-256.png",
  "/icon-512.png",
  "/icon-512-maskable.png",
  "/favicon.png",
];
const FETCH_TIMEOUT_MS = 7000;

self.addEventListener("install", (e) => {
  // allSettled (NOT addAll): addAll is atomic, so one flaky asset fetch would reject the whole
  // install and block this SW from ever activating — which is what let a bad state persist. Tolerate
  // individual failures so a new SW always takes over.
  e.waitUntil(
    caches.open(CACHE)
      .then((c) => Promise.allSettled(STATIC.map((u) => c.add(u))))
      .then(() => self.skipWaiting())
  );
});

self.addEventListener("activate", (e) => {
  e.waitUntil(
    caches.keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  );
});

// fetch() with a timeout so a stalled request (e.g. an HTTP/1.1 connection-pool stall behind the
// long-lived /ws socket) can't hang a navigation forever — it fails fast and the browser retries.
function fetchWithTimeout(req, ms) {
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => reject(new Error("sw-timeout")), ms);
    fetch(req).then(
      (r) => { clearTimeout(t); resolve(r); },
      (err) => { clearTimeout(t); reject(err); }
    );
  });
}

self.addEventListener("fetch", (e) => {
  const req = e.request;
  const url = new URL(req.url);
  // Leave cross-origin, non-GET, /version (the self-heal's source of truth), and /ws to the browser.
  if (req.method !== "GET" || url.origin !== self.location.origin) return;
  if (url.pathname === "/version" || url.pathname === "/ws") return;

  const isStatic = STATIC.indexOf(url.pathname) !== -1;

  if (!isStatic) {
    // SHELL: network-only, timed, NEVER from cache — no stale/cross-build shell can be handed back.
    e.respondWith(fetchWithTimeout(req, FETCH_TIMEOUT_MS).catch(() => Response.error()));
    return;
  }

  // STATIC asset: stale-while-revalidate. Serve the cached copy instantly if present and refresh in
  // the background; otherwise go to the network and cache a clean 200.
  e.respondWith(
    caches.match(req).then((hit) => {
      const net = fetchWithTimeout(req, FETCH_TIMEOUT_MS)
        .then((res) => {
          if (res && res.status === 200) {
            const copy = res.clone();
            caches.open(CACHE).then((c) => c.put(req, copy)).catch(() => {});
          }
          return res;
        });
      if (hit) {
        net.catch(() => {}); // background refresh; ignore failures
        return hit;
      }
      return net.catch(() => Response.error());
    })
  );
});
