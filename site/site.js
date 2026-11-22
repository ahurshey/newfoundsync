// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Alex Hurshman and the Newfoundsync contributors.

// The page is complete without this file. It adds: the light/dark switch, a hint for which download
// fits this computer, download links that follow the newest GitHub release, and the sync diagram.
(function () {
  "use strict";
  var root = document.documentElement;
  var $ = function (sel, el) { return (el || document).querySelector(sel); };
  var $$ = function (sel, el) { return Array.prototype.slice.call((el || document).querySelectorAll(sel)); };

  // ---- light / dark ---------------------------------------------------------------------------
  var themeBtn = $("#theme");
  if (themeBtn) {
    themeBtn.addEventListener("click", function () {
      var dark = root.dataset.theme
        ? root.dataset.theme === "dark"
        : window.matchMedia("(prefers-color-scheme: dark)").matches;
      root.dataset.theme = dark ? "light" : "dark";
      try { localStorage.setItem("nfs_site_theme", root.dataset.theme); } catch (e) {}
    });
  }

  // ---- which download is for this computer ----------------------------------------------------
  var ua = navigator.userAgent || "";
  var plat = (navigator.userAgentData && navigator.userAgentData.platform) || navigator.platform || "";
  var handheld = /Android|iPhone|iPad|iPod/i.test(ua) || (/Mac/i.test(plat) && navigator.maxTouchPoints > 1);
  var os = handheld ? null : /Win/i.test(plat) ? "windows" : /Mac/i.test(plat) ? "mac" : /Linux|X11/i.test(plat) ? "linux" : null;
  var osName = { windows: "Windows", linux: "Linux", mac: "macOS" };
  var heroBtn = $("#hero-dl");
  if (heroBtn) heroBtn.textContent = os ? "Download for " + osName[os] : handheld ? "Get it for your computer" : "Download";
  if (os) {
    var mine = $('#dl-cards [data-os="' + os + '"]');
    if (mine) mine.classList.add("yours");
  }

  // ---- download links follow the newest release -----------------------------------------------
  // The links in the HTML point at the release that was current when the page was written, so they
  // work with no script and when GitHub's API is unreachable. This replaces them with the newest
  // release that has a build for each system. A build older than FEATURES_SINCE keeps a note saying
  // it does not have what the screenshots show; the note goes once that system catches up.
  var REPO = "ahurshey/newfoundsync";
  var ASSET_PREFIX = "https://github.com/" + REPO + "/releases/download/";
  var FEATURES_SINCE = "0.1.4";
  var match = { windows: /\.exe$/i, linux: /\.flatpak$/i, mac: /\.pkg$/i };

  function cmpVersion(a, b) {
    var x = String(a).replace(/^v/, "").split("."), y = String(b).replace(/^v/, "").split(".");
    for (var i = 0; i < Math.max(x.length, y.length); i++) {
      var d = (parseInt(x[i], 10) || 0) - (parseInt(y[i], 10) || 0);
      if (d) return d;
    }
    return 0;
  }
  function setText(sel, text) { $$(sel).forEach(function (el) { el.textContent = text; }); }

  function applyReleases(list) {
    var rels = list.filter(function (r) { return r && !r.draft && !r.prerelease && r.tag_name; })
                   .sort(function (a, b) { return cmpVersion(b.tag_name, a.tag_name); });
    if (!rels.length) return;
    Object.keys(match).forEach(function (key) {
      for (var i = 0; i < rels.length; i++) {
        var asset = (rels[i].assets || []).filter(function (a) {
          return match[key].test(a.name) && String(a.browser_download_url).indexOf(ASSET_PREFIX) === 0;
        })[0];
        if (!asset) continue;
        var ver = rels[i].tag_name.replace(/^v/, "");
        $$('[data-dl="' + key + '"]').forEach(function (a) { a.href = asset.browser_download_url; });
        setText('[data-ver="' + key + '"]', ver);
        setText('[data-size="' + key + '"]', Math.round(asset.size / 1e6) + " MB");
        setText('[data-file="' + key + '"]', asset.name);
        $$('[data-older="' + key + '"]').forEach(function (el) { el.hidden = cmpVersion(ver, FEATURES_SINCE) >= 0; });
        break;
      }
    });
  }
  if (window.fetch) {
    fetch("https://api.github.com/repos/" + REPO + "/releases?per_page=20", { headers: { Accept: "application/vnd.github+json" } })
      .then(function (r) { return r.ok ? r.json() : Promise.reject(); })
      .then(function (list) { if (Array.isArray(list)) applyReleases(list); })
      .catch(function () { /* the links already in the page stand */ });
  }

  // ---- the sync diagram -----------------------------------------------------------------------
  // One moment of sound, four devices. Time runs left to right. The delays are redrawn every pass,
  // because the point is that they are never the same twice and the play time still is.
  var demo = $("#demo");
  if (!demo || !demo.querySelector("svg")) return;
  var NS = "http://www.w3.org/2000/svg";
  var svg = demo.querySelector("svg");
  var devices = [
    { name: "Office laptop", note: "wired", lo: 0.07, hi: 0.13, hz: 220.0, pan: -0.6 },
    { name: "Kitchen tablet", note: "Wi-Fi", lo: 0.2, hi: 0.36, hz: 277.18, pan: -0.2 },
    { name: "Living room TV", note: "Wi-Fi, slow speakers", lo: 0.42, hi: 0.58, hz: 329.63, pan: 0.2 },
    { name: "Patio speaker", note: "Bluetooth", lo: 0.68, hi: 0.86, hz: 440.0, pan: 0.6 },
  ];
  function el(name, attrs, parent, text) {
    var n = document.createElementNS(NS, name);
    for (var k in attrs) n.setAttribute(k, attrs[k]);
    if (text) n.textContent = text;
    parent.appendChild(n);
    return n;
  }
  // Two drawings of the same thing. Wide: names in a column on the left. Narrow (a phone): names
  // above each lane, because the wide one scaled down to 300 px leaves 6 px text.
  var X0, X1, SPK, now, drawn = null;
  var caption = $("#demo-caption", demo);
  var narrowMq = window.matchMedia("(max-width: 560px)");
  function build() {
    var narrow = narrowMq.matches;
    if (drawn) drawn.remove();
    drawn = el("g", {}, svg);
    var W = narrow ? 340 : 640, top = narrow ? 70 : 70, gap = narrow ? 70 : 52;
    X0 = narrow ? 12 : 170; X1 = narrow ? 262 : 540; SPK = narrow ? 308 : 590;
    var bottom = top + 3 * gap + (narrow ? 22 : 42);
    svg.setAttribute("viewBox", "0 0 " + W + " " + (bottom + 34));
    var axis = el("g", { class: "axis" }, drawn);
    el("line", { x1: X0, y1: 34, x2: X0, y2: bottom }, axis);
    el("line", { class: "deadline", x1: X1, y1: 34, x2: X1, y2: bottom }, axis);
    el("text", { x: narrow ? X0 : X0, y: 22, "text-anchor": narrow ? "start" : "middle" }, axis, "leaves the computer");
    el("text", { class: "deadline-label", x: narrow ? X1 + 8 : X1, y: 22, "text-anchor": narrow ? "end" : "middle" }, axis, "shared play time");
    devices.forEach(function (d, i) {
      var y = top + i * gap;
      var g = el("g", { class: "lane" }, drawn);
      if (narrow) {
        var label = el("text", { x: X0 + 8, y: y - 13 }, g);
        el("tspan", { class: "lane-name" }, label, d.name);
        el("tspan", { class: "lane-note", dx: 7 }, label, d.note);
      } else {
        el("text", { class: "lane-name", x: 8, y: y - 1 }, g, d.name);
        el("text", { class: "lane-note", x: 8, y: y + 15 }, g, d.note);
      }
      el("line", { class: "rail", x1: X0, y1: y, x2: X1, y2: y }, g);
      d.transit = el("line", { class: "transit", x1: X0, y1: y, x2: X0, y2: y }, g);
      d.wait = el("line", { class: "wait", x1: X0, y1: y, x2: X0, y2: y, visibility: "hidden" }, g);
      d.dot = el("circle", { class: "arrive", cx: X0, cy: y, r: 4.5, visibility: "hidden" }, g);
      d.pulse = el("circle", { class: "pulse", cx: SPK, cy: y, r: 17 }, g);
      el("circle", { class: "spk", cx: SPK, cy: y, r: 17 }, g);
      el("path", { class: "spk-cone", d: "M" + (SPK - 8) + " " + (y - 4) + "h4l6-5v18l-6-5h-4z" }, g);
      el("path", { class: "spk-wave", d: "M" + (SPK + 5) + " " + (y - 5) + "a7 7 0 0 1 0 10" }, g);
      d.g = g;
    });
    now = el("line", { class: "now", x1: X0, y1: 34, x2: X0, y2: bottom }, drawn);
    var key = el("g", { class: "key", transform: "translate(" + X0 + " " + (bottom + 20) + ")" }, drawn);
    el("line", { class: "transit", x1: 0, y1: 0, x2: 26, y2: 0 }, key);
    el("text", { x: 34, y: 4 }, key, "on its way");
    el("line", { class: "wait", x1: 118, y1: 0, x2: 144, y2: 0 }, key);
    el("text", { class: "wait-label", x: 152, y: 4 }, key, "waiting in the buffer");
  }
  build();
  demo.hidden = false;

  var mode = "sync";
  var reduced = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  var SWEEP = 2600, HOLD = 1100;
  var started = 0, raf = 0, visible = false;
  var audio = null, soundOn = false;

  function pluck(d, when) {
    if (!soundOn || !audio) return;
    var t = when || audio.currentTime;
    var osc = audio.createOscillator(), gain = audio.createGain();
    osc.type = "triangle"; osc.frequency.value = d.hz;
    gain.gain.setValueAtTime(0.0001, t);
    gain.gain.exponentialRampToValueAtTime(0.16, t + 0.012);
    gain.gain.exponentialRampToValueAtTime(0.0001, t + 0.55);
    var out = gain;
    if (audio.createStereoPanner) { out = audio.createStereoPanner(); out.pan.value = d.pan; gain.connect(out); }
    osc.connect(gain); out.connect(audio.destination);
    osc.start(t); osc.stop(t + 0.6);
  }
  function play(d) {
    d.played = true;
    d.g.classList.add("playing");
    if (!reduced && d.pulse.animate) {
      d.pulse.animate([{ transform: "scale(1)", opacity: 0.9 }, { transform: "scale(2.1)", opacity: 0 }], { duration: 650, easing: "ease-out" });
    }
  }
  function newPass() {
    devices.forEach(function (d) {
      d.at = d.lo + Math.random() * (d.hi - d.lo);
      d.played = false;
      d.g.classList.remove("playing");
    });
  }
  function draw(u) { // u: 0 at the moment the sound leaves, 1 at the shared play time
    var x = X0 + Math.min(u, 1) * (X1 - X0);
    now.setAttribute("x1", x); now.setAttribute("x2", x);
    now.style.visibility = u >= 1 ? "hidden" : "visible";
    var together = [];
    devices.forEach(function (d) {
      var ax = X0 + d.at * (X1 - X0), arrived = u >= d.at;
      d.transit.setAttribute("x2", Math.min(x, ax));
      d.dot.setAttribute("cx", ax);
      d.dot.setAttribute("visibility", arrived ? "visible" : "hidden");
      var waiting = mode === "sync" && arrived;
      d.wait.setAttribute("visibility", waiting ? "visible" : "hidden");
      if (waiting) { d.wait.setAttribute("x1", ax + 9); d.wait.setAttribute("x2", Math.max(ax + 9, x - 5)); }
      var due = mode === "sync" ? u >= 1 : arrived;
      if (due && !d.played) { play(d); if (mode === "sync") together.push(d); else pluck(d); }
    });
    if (together.length && soundOn && audio) { // one start time, so the chord is as tight as the picture
      var t = audio.currentTime + 0.02;
      together.forEach(function (d) { pluck(d, t); });
    }
  }
  function frame(ts) {
    if (!started) { started = ts; newPass(); }
    var t = ts - started;
    if (t > SWEEP + HOLD) { started = ts; newPass(); t = 0; }
    draw(t / SWEEP);
    raf = visible ? requestAnimationFrame(frame) : 0;
  }
  function still() { // reduced motion: the finished picture, no sweep
    newPass();
    draw(1);
  }
  function setMode(next) {
    mode = next;
    demo.classList.toggle("raw", mode === "raw");
    $$(".seg button", demo).forEach(function (b) { b.setAttribute("aria-pressed", String(b.dataset.mode === mode)); });
    caption.textContent = mode === "sync"
      ? "All four play at the same instant."
      : "Each plays whenever the sound happens to arrive.";
    started = 0;
    if (reduced) still();
  }
  $$(".seg button", demo).forEach(function (b) { b.addEventListener("click", function () { setMode(b.dataset.mode); }); });

  function onLayout() { build(); started = 0; if (reduced) still(); else { newPass(); draw(0); } }
  if (narrowMq.addEventListener) narrowMq.addEventListener("change", onLayout);

  var soundBtn = $("#demo-sound", demo);
  if (reduced || !(window.AudioContext || window.webkitAudioContext)) soundBtn.hidden = true;
  soundBtn.addEventListener("click", function () {
    soundOn = !soundOn;
    if (soundOn && !audio) audio = new (window.AudioContext || window.webkitAudioContext)();
    if (audio && audio.state === "suspended") audio.resume();
    soundBtn.setAttribute("aria-pressed", String(soundOn));
    soundBtn.textContent = soundOn ? "Sound on" : "Sound off";
  });

  if (reduced) { still(); return; }
  newPass(); draw(0);
  if ("IntersectionObserver" in window) {
    new IntersectionObserver(function (entries) {
      visible = entries[0].isIntersecting;
      if (visible && !raf) { started = 0; raf = requestAnimationFrame(frame); }
    }, { threshold: 0.25 }).observe(demo);
  } else {
    visible = true; raf = requestAnimationFrame(frame);
  }
  document.addEventListener("visibilitychange", function () { if (!document.hidden) started = 0; });
})();
