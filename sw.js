// Service worker for Verus Explorer cold-load caching. Intercepts the
// heavy static assets (verus_explorer_bg.wasm, z3.wasm, the libs/*.gz
// rmeta bundle, editor.js, styles, examples) and serves them cache-first
// on repeat visits. The compiled-wasm-module cache lives separately in
// IndexedDB (see `compileCachedWasm` in index.html) and skips the compile
// step too; this file only handles the network fetch layer.
//
// Cache invalidation: bump VERSION on any deploy where cached asset bytes
// change and you want old clients to refetch. The `activate` handler
// deletes caches whose names don't match the current VERSION. Navigations
// (index.html itself) are deliberately not cached — they always hit the
// network so a new deploy can swap in a new sw.js VERSION.
const VERSION = 'v1';
const CACHE_NAME = `verus-explorer-${VERSION}`;

// Regex allowlist instead of a precache manifest: lets new libs appear
// without having to mirror the `WASM_LIBS` list here, and keeps the SW
// out of paths we don't want to intercept (HTML, favicon, screenshots).
const CACHEABLE = [
  /\/verus_explorer_bg\.wasm$/,
  /\/verus_explorer\.js$/,
  /\/editor\.js$/,
  /\/styles\.css$/,
  /\/z3\/z3\.(js|wasm)$/,
  /\/libs\/.*\.(gz|rmeta|vir)$/,
  /\/examples\/[^/]+\.rs$/,
];

self.addEventListener('install', () => {
  // Take over as soon as we install. Paired with `clients.claim()` below
  // so the first SW-enabled visit benefits from the cache on subsequent
  // in-session fetches instead of waiting for a reload.
  self.skipWaiting();
});

self.addEventListener('activate', (e) => {
  e.waitUntil((async () => {
    const keys = await caches.keys();
    await Promise.all(keys.filter(k => k !== CACHE_NAME).map(k => caches.delete(k)));
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (e) => {
  if (e.request.method !== 'GET') return;
  const url = new URL(e.request.url);
  if (url.origin !== location.origin) return;
  if (!CACHEABLE.some(re => re.test(url.pathname))) return;

  e.respondWith((async () => {
    const cache = await caches.open(CACHE_NAME);
    const cached = await cache.match(e.request);
    if (cached) return cached;
    const resp = await fetch(e.request);
    if (resp.ok) cache.put(e.request, resp.clone()).catch(() => {});
    return resp;
  })());
});
