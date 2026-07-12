// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

// SELF-DESTRUCT service worker. Newfoundsync no longer uses a service worker: a caching layer on a
// LAN app that needs a live server anyway only ever produced stale / half-broken shells that a plain
// reload couldn't fix (the SW served the reload too). Any browser still holding a registration
// fetches THIS on its next update check (browsers re-check /sw.js on navigation); it unregisters
// itself, drops every cache, and reloads its windows onto the plain, always-fresh, network-served
// client. app.js no longer registers a worker, so once this has run no new one is ever created.
//
// There is deliberately NO fetch handler — nothing is intercepted or cached; every request goes
// straight to the network.

self.addEventListener("install", () => self.skipWaiting());

self.addEventListener("activate", (e) => {
  e.waitUntil((async () => {
    try {
      const keys = await caches.keys();
      await Promise.all(keys.map((k) => caches.delete(k)));
    } catch (err) {}
    try {
      const wins = await self.clients.matchAll({ type: "window" });
      for (const c of wins) c.navigate(c.url); // reload each open page onto the plain client
    } catch (err) {}
    try {
      await self.registration.unregister();
    } catch (err) {}
  })());
});
