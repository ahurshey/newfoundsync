// Guards the single-source-of-truth property that makes calibration work at all.
//
// The reference device PLAYS a coded signal; the follower CORRELATES the microphone against its own
// copy of that signal, generated inside a Web Worker. If those two signals ever differ — by one
// coefficient, one chip, one sample of phase — correlation stops locking and calibration returns
// confident nonsense rather than failing loudly.
//
// app.js secures this by `.toString()`-ing the very function objects it uses on the main thread and
// injecting that source into the worker. It's a neat trick, and a fragile one: it silently breaks the
// moment any of those functions starts referencing something from module scope, because the worker copy
// has no such scope. This test executes the worker's ACTUAL injected source in isolation and asserts it
// produces byte-identical output to the module — which is the property that actually matters.
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const WEB = path.join(__dirname, '..', '..', 'crates', 'desktop', 'web');
const DSP = require(path.join(WEB, 'nfs-dsp.js'));
const APP_JS = fs.readFileSync(path.join(WEB, 'app.js'), 'utf8');
const DSP_SRC = fs.readFileSync(path.join(WEB, 'nfs-dsp.js'), 'utf8');

/** The list of functions app.js serializes into the worker, read from app.js itself. */
function injectedFunctionNames() {
  const m = APP_JS.match(/const CALCODE_SRC = \[([^\]]+)\]/);
  assert.ok(m, 'app.js should still build CALCODE_SRC from a list of function objects');
  return m[1].split(',').map((s) => s.trim()).filter(Boolean);
}

test('app.js still injects the DSP into the worker by serializing real function objects', () => {
  const names = injectedFunctionNames();
  assert.ok(names.length >= 5, `expected the DSP helpers, got ${JSON.stringify(names)}`);
  for (const n of names) {
    assert.equal(typeof DSP[n], 'function', `${n} must be exported by nfs-dsp.js — app.js injects it`);
  }
  assert.match(APP_JS, /\.map\(\(f\) => f\.toString\(\)\)/, 'injection must still be by .toString()');
});

test('the worker copy is self-contained: no function reaches outside its own body', () => {
  // This is the failure mode the .toString() trick hides. A function that closes over a module-scope
  // constant works perfectly on the main thread and throws (or worse, reads undefined) in the worker.
  const names = injectedFunctionNames();
  for (const n of names) {
    // Strip `Math.X` / `Foo.bar` property accesses first — `Math.SQRT1_2` is a builtin, not a
    // module-scope reference, and matching it was a false positive in an earlier version of this test.
    const src = DSP[n].toString().replace(/\.\s*[A-Za-z_$][\w$]*/g, '');
    // What's left that looks like OUR module scope: the app's known globals, or bare CONSTANT_CASE.
    const suspicious = (src.match(/\b(CALCFG|calib|cfg|els|ac|CLOCK_[A-Z_]+|[A-Z][A-Z0-9_]{3,})\b/g) || [])
      .filter((id) => !names.includes(id));
    assert.deepEqual(
      [...new Set(suspicious)],
      [],
      `${n} references module scope, which does not exist in the worker: ${suspicious}`
    );
  }
});

test('worker-serialized DSP produces byte-identical output to the module', () => {
  // Rebuild exactly what app.js sends to the worker, then run it in a bare sandbox that has NOTHING
  // but the JS builtins — the same starvation the real worker imposes.
  const names = injectedFunctionNames();
  const injected = names.map((n) => DSP[n].toString()).join('\n');
  const sandbox = { result: null };
  vm.createContext(sandbox);

  const probe = `
    ${injected}
    const seed = 20260725, n = 4096, rate = 16000, f0 = 500, f1 = 4000;
    const code = calBuildCode(seed, n, rate, f0, f1);
    result = {
      code: Array.from(code),
      prng: Array.from(calCodePrng(seed, 256)),
      lp: calBiquadLP(rate, 1000),
      hp: calBiquadHP(rate, 1000),
    };
  `;
  // If any injected function depended on module scope, this throws — which is the point.
  vm.runInContext(probe, sandbox, { timeout: 10_000 });

  const mine = {
    code: Array.from(DSP.calBuildCode(20260725, 4096, 16000, 500, 4000)),
    prng: Array.from(DSP.calCodePrng(20260725, 256)),
    lp: DSP.calBiquadLP(16000, 1000),
    hp: DSP.calBiquadHP(16000, 1000),
  };

  // Re-wrap the sandbox arrays in THIS realm before comparing: values crossing a vm context keep
  // their own realm's Array.prototype, so a strict deep-equal fails on the prototype alone even when
  // every element is identical. Comparing the numbers is what we actually mean.
  const theirs = {
    code: Array.from(sandbox.result.code),
    prng: Array.from(sandbox.result.prng),
    lp: Array.from(sandbox.result.lp),
    hp: Array.from(sandbox.result.hp),
  };

  assert.deepEqual(theirs.prng, mine.prng, 'worker PRNG must match the module exactly');
  assert.deepEqual(theirs.lp, mine.lp, 'worker LP coefficients must match');
  assert.deepEqual(theirs.hp, mine.hp, 'worker HP coefficients must match');
  assert.deepEqual(
    theirs.code,
    mine.code,
    'the signal the reference PLAYS and the template the follower CORRELATES must be identical'
  );
});

test('index.html loads nfs-dsp.js before app.js', () => {
  // Order is load-bearing: app.js calls these as globals at module level and serializes them into the
  // worker. Reversed or deferred, the client throws during startup — the exact class of failure that
  // once shipped here (a TDZ crash that aborted all of init).
  const html = fs.readFileSync(path.join(WEB, 'index.html'), 'utf8');
  const dsp = html.indexOf('/nfs-dsp.js');
  const app = html.indexOf('/app.js');
  assert.ok(dsp !== -1, 'index.html must load /nfs-dsp.js');
  assert.ok(app !== -1, 'index.html must load /app.js');
  assert.ok(dsp < app, 'nfs-dsp.js must be loaded BEFORE app.js');
  // Inspect the TAG itself, not the text around it — an earlier version scanned the preceding bytes and
  // was tripped by a comment that merely mentioned "deferred/async".
  const tag = html.slice(html.lastIndexOf('<script', dsp), html.indexOf('>', dsp) + 1);
  assert.match(tag, /nfs-dsp\.js/, 'should have located the nfs-dsp.js script tag');
  assert.doesNotMatch(
    tag,
    /\b(defer|async)\b/,
    `nfs-dsp.js must not be defer/async — app.js needs it synchronously (tag: ${tag})`
  );
});

test('nfs-dsp.js exports for node AND exposes globals for the browser', () => {
  // The dual-target shim is what lets one file serve the page, the worker, and these tests. Losing the
  // browser half would break the client while every one of these tests kept passing.
  assert.match(DSP_SRC, /module\.exports\s*=\s*api/, 'must export for node --test');
  assert.match(DSP_SRC, /for \(const k in api\) root\[k\] = api\[k\]/, 'must expose browser globals');
});
