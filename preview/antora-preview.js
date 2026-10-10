'use strict'

// Live preview for Antora sites.
//
// Builds the site from HEAD (the working tree of local content sources) using the project's
// own Antora, keeps the result in memory and serves it over HTTP. Pages are converted when
// first requested, so large sites are ready quickly. When a file changes — saved to disk, or
// unsaved text posted by antora-ls — only the affected page is re-rendered and pushed to open
// browsers, which swap in the new article without a reload.
//
// Usage: bun antora-preview.js <playbook> [--port N]
// Prints `ANTORA_PREVIEW <url> <token>` on stdout as soon as the server listens (browsers get a
// progress page until the first build is done), then `ANTORA_PREVIEW_STATUS <json>` lines as
// builds progress, and `ANTORA_PREVIEW_REVEAL <json>` when a block is double-clicked in the browser
// (the editor shows its source line); logs go to stderr. Only Node built-ins are used, so it runs
// under Bun or Node.

const fs = require('node:fs')
const http = require('node:http')
const path = require('node:path')
const crypto = require('node:crypto')
const { createRequire } = require('node:module')

// Shared with the copy of this module that Antora loads as an extension.
const S = (globalThis.__antoraPreview ??= { clients: new Set(), overlays: new Map(), status: {} })

// --- Antora extension -------------------------------------------------------------------

/** Captures the build's catalogs instead of publishing files. Registered by build(). */
function register () {
  const out = S.pending
  if (!out) return // loaded outside the preview server

  // Defer converting and composing pages to the first request for each: reading every page's
  // header (for xref text and navigation) is a fraction of the work of rendering it.
  this.once('contextStarted', () => {
    const { convertDocument, createPageComposer } = this.getFunctions()
    out.convertDocument = convertDocument
    this.replaceFunctions({
      convertDocument (file) {
        file.contents = Buffer.alloc(0)
        file.mediaType = 'text/html'
        return file
      },
      createPageComposer (playbook, contentCatalog, uiCatalog, env = playbook.env) {
        // Keep env as it is now: extensions may change it before pages are composed on request.
        env = Object.assign({}, env)
        const composer = createPageComposer(Object.assign({}, playbook, { env }), contentCatalog, uiCatalog, env)
        out.composePage = composer.composePage
        return Object.assign({}, composer, { composePage: (page) => page })
      },
    })
  })
  const phase = (name) => () => setStatus({ phase: name })
  this.once('playbookBuilt', phase('Reading content sources'))
  this.once('contentAggregated', phase('Classifying content'))
  this.once('documentsConverted', phase('Building navigation'))
  this.once('navigationBuilt', phase('Preparing pages'))

  this.once('contentClassified', ({ contentCatalog }) => {
    setStatus({ phase: 'Reading page headers' })
    for (const component of contentCatalog.getComponents()) {
      for (const cv of component.versions) {
        cv.asciidoc = Object.assign({}, cv.asciidoc, {
          sourcemap: true,
          extensions: [...(cv.asciidoc?.extensions || []), lineMarker],
        })
      }
    }
    out.sources = new Map()
    out.byPath = new Map()
    for (const file of contentCatalog.getFiles()) {
      const abspath = file.src.abspath && realpath(file.src.abspath)
      if (abspath) out.byPath.set(abspath, file)
      const overlay = abspath && S.overlays.get(abspath)
      if (overlay != null) file.contents = Buffer.from(overlay)
      if (file.src.mediaType === 'text/asciidoc') out.sources.set(file, file.contents)
    }
  })

  this.once('navigationBuilt', ({ navigationCatalog }) => {
    out.navigationCatalog = navigationCatalog
  })

  this.once('beforePublish', ({ contentCatalog, uiCatalog, siteCatalog, siteAsciiDocConfig }) => {
    Object.assign(out, { contentCatalog, siteAsciiDocConfig, fxns: this.getFunctions() })
    out.routes = new Map()
    for (const catalog of [contentCatalog, uiCatalog, siteCatalog]) {
      for (const file of catalog.getFiles()) if (file.out) out.routes.set(file.out.path, file)
    }
    out.dirty = new Set(contentCatalog.getPages(isPage))
    // Antora drops page sources after converting; pages included elsewhere still need theirs.
    for (const page of out.dirty) if ('page-partial' in page.asciidoc.attributes) page.src.contents = out.sources.get(page)
    this.stop() // everything is served from memory; skip writing the site to disk
  })
}

/** Tags every block of the page itself with its source line, for cursor sync. */
const lineMarker = {
  register (registry) {
    registry.treeProcessor(function () {
      this.process((doc) => {
        const docfile = doc.getAttribute('docfile')
        for (const block of doc.findBy()) {
          const loc = block.getSourceLocation?.()
          if (!loc || block.getContext() === 'document') continue
          if ((loc.getFile() ?? docfile) !== docfile) continue // from an include
          block.addRole('preview-line-' + loc.getLineNumber())
        }
      })
    })
  },
}

// --- Building ---------------------------------------------------------------------------

/** Require Antora packages the way the project resolves them (pnpm keeps them nested). */
function antoraRequire (playbookFile) {
  const from = (base, id) => createRequire(base).resolve(id)
  let generator
  try {
    generator = from(playbookFile, '@antora/site-generator')
  } catch {
    generator = from(from(playbookFile, 'antora/package.json'), '@antora/site-generator')
  }
  return createRequire(generator)
}

/** Canonical path, so paths from the editor match the catalog despite symlinks. */
function realpath (p) {
  try {
    return fs.realpathSync.native(p)
  } catch {
    return path.resolve(p)
  }
}

const isLocal = (url) => !/^[a-z][a-z0-9+.-]*:\/\//i.test(url) && !/^[^/\\]+@[^:]+:/.test(url)

/** Point local content sources at HEAD (the working tree), dropping ones HEAD lacks. */
function useHead (playbook) {
  const seen = new Set()
  playbook.content.sources = playbook.content.sources.filter((src) => {
    if (!isLocal(src.url)) return true
    const dir = path.resolve(playbook.dir || '.', src.url)
    const starts = src.startPaths ?? (src.startPath != null ? [src.startPath] : [''])
    const present = [].concat(starts).filter((p) => /[*{]/.test(p) || fs.existsSync(path.join(dir, p)))
    if (!present.length) {
      log(`skipping ${src.url} (${[].concat(starts).join(', ')}): not in HEAD`)
      return false
    }
    const key = dir + '\0' + present.join('\0')
    if (seen.has(key)) return false
    seen.add(key)
    Object.assign(src, { branches: ['HEAD'], worktrees: true })
    if (src.startPaths) src.startPaths = present
    delete src.tags
    return true
  })
}

async function build () {
  const req = S.req
  const started = Date.now()
  S.changed = new Set()
  setStatus({ state: 'building', phase: 'Loading playbook', started, error: undefined })
  const playbook = req('@antora/playbook-builder')(['--playbook', S.playbookFile], process.env)
  // stdout carries the handshake line; Antora's log goes to stderr, readable for the editor's log.
  playbook.runtime.log = Object.assign({}, playbook.runtime.log, { format: 'pretty', destination: { file: 'stderr' } })
  try {
    req('@antora/logger').configureLogger(playbook.runtime.log, path.dirname(S.playbookFile))
  } catch {}
  useHead(playbook)
  S.playbook = playbook
  S.localDirs = playbook.content.sources.filter((s) => isLocal(s.url)).flatMap((s) => {
    const dir = path.resolve(playbook.dir || '.', s.url)
    return [].concat(s.startPaths ?? s.startPath ?? '').map((p) => path.join(dir, p.split(/[*{]/)[0]))
  })
  const supplemental = playbook.ui?.supplementalFiles
  S.uiDir = typeof supplemental === 'string' ? path.resolve(playbook.dir || '.', supplemental) : undefined
  playbook.antora ??= {}
  playbook.antora.extensions = [...(playbook.antora.extensions || []), { require: __filename }]
  playbook.runtime.quiet = true

  const out = (S.pending = {})
  try {
    await req('@antora/site-generator')(playbook)
  } finally {
    S.pending = undefined
  }
  if (!out.routes) throw new Error('site generation stopped before pages were composed')
  S.site = out
  // Edits that arrived after the build read its files: apply them to the new site.
  const changed = S.changed
  S.changed = undefined
  for (const abspath of changed) update(abspath, S.overlays.get(abspath) ?? null)
  log(`built ${[...out.routes.keys()].filter((p) => p.endsWith('.html')).length} pages in ${Date.now() - started} ms`)
}

/** Build, one at a time: changes during a build queue one more. Keeps serving the old site. */
let building = false
let rebuildQueued = false
async function runBuild () {
  if (building) return (rebuildQueued = true)
  building = true
  rebuildQueued = false
  try {
    await build()
    setStatus({ state: 'ready', phase: undefined })
    broadcast('reload', {})
  } catch (err) {
    S.changed = undefined
    log('build failed: ' + (err.stack || err))
    setStatus({ state: 'failed', phase: undefined, error: String(err.message || err) })
  } finally {
    building = false
    watch()
    if (rebuildQueued) scheduleRebuild()
  }
}

let rebuildTimer
function scheduleRebuild () {
  clearTimeout(rebuildTimer)
  rebuildTimer = setTimeout(runBuild, 300)
}

/** Build progress, for the browser and (on stdout) for antora-ls. */
function setStatus (patch) {
  Object.assign(S.status, patch)
  broadcast('status', S.status)
  process.stdout.write(`${STATUS} ${JSON.stringify(S.status)}\n`)
}
const STATUS = 'ANTORA_PREVIEW_STATUS'

// --- Incremental updates ------------------------------------------------------------------

function renderPage (page) {
  const site = S.site
  page.contents = site.sources.get(page)
  page.mediaType = 'text/asciidoc'
  delete page.asciidoc
  const cv = site.contentCatalog.getComponentVersion(page.src.component, page.src.version)
  site.convertDocument(page, site.contentCatalog, cv?.asciidoc || site.siteAsciiDocConfig)
  site.composePage(page, site.contentCatalog, site.navigationCatalog)
  site.dirty.delete(page)
}

const isPage = (file) => file.src.family === 'page' && file.out && file.src.mediaType === 'text/asciidoc'

/** Apply new contents for a file (text = null means: read it from disk). */
function update (abspath, text) {
  abspath = realpath(abspath)
  if (text == null) S.overlays.delete(abspath)
  else S.overlays.set(abspath, text)
  S.changed?.add(abspath)

  const site = S.site
  if (!site) return text == null && scheduleRebuild() // the first build failed: retry on save
  // Files outside the catalog (new ones, antora.yml, ...) are left to the watcher, which
  // rebuilds when they are created or saved.
  const file = site.byPath.get(abspath)
  if (!file) return
  let contents
  try {
    contents = text != null ? Buffer.from(text) : fs.readFileSync(abspath)
  } catch {
    return scheduleRebuild() // deleted
  }

  const family = file.src.family
  if (isPage(file)) {
    site.sources.set(file, contents)
    renderPage(file)
    pushPage(file, false)
  } else if (family === 'nav' || family === 'partial' || family === 'example' || site.sources.has(file)) {
    file.contents = contents
    if (site.sources.has(file)) site.sources.set(file, contents)
    if (family === 'nav') site.navigationCatalog = site.fxns.buildNavigation(site.contentCatalog, site.siteAsciiDocConfig)
    // Any page may include this file; re-render viewed pages now and the rest when requested.
    for (const page of site.contentCatalog.getPages(isPage)) site.dirty.add(page)
    for (const page of viewedPages()) {
      renderPage(page)
      pushPage(page, family === 'nav')
    }
  } else {
    file.contents = contents // images, attachments
    broadcast('reload', {})
  }
}

function viewedPages () {
  const pages = new Set()
  for (const c of S.clients) {
    const file = S.site.routes.get(c.page)
    if (file && isPage(file)) pages.add(file)
  }
  return pages
}

function pushPage (page, navChanged) {
  const data = { html: page.contents.toString(), nav: navChanged }
  for (const c of S.clients) if (c.page === page.out.path) send(c, 'page', data)
}

/** Follow the editor: show the page being edited and scroll to the cursor line. */
function cursor (abspath, line) {
  const file = S.site?.byPath.get(realpath(abspath))
  if (!file || !isPage(file)) return
  if (S.site.dirty.has(file)) renderPage(file)
  broadcast('cursor', { page: file.out.path, line })
}

// --- File watching --------------------------------------------------------------------------

/** Whether a change to `abspath` (reported by the watcher, not an edit) needs a full build. */
function needsRebuild (abspath, event) {
  if (!S.site || S.site.byPath.has(abspath)) return true // retrying a failed build; deleted
  if (path.basename(abspath) === 'antora.yml' || (S.uiDir && abspath.startsWith(S.uiDir + path.sep))) return true
  return event === 'rename' && abspath.split(path.sep).includes('modules') // new content file
}

function watch () {
  if (S.watching || !S.localDirs) return
  S.watching = true
  for (const dir of new Set([...S.localDirs, ...(S.uiDir ? [S.uiDir] : [])])) {
    try {
      fs.watch(dir, { recursive: true }, (event, name) => {
        if (!name || name.split(path.sep).some((s) => s.startsWith('.'))) return
        const abspath = realpath(path.join(dir, name))
        // Check the file itself: macOS reports most saves as renames.
        if (S.site?.byPath.has(abspath) && fs.existsSync(abspath)) {
          if (!S.overlays.has(abspath)) update(abspath, null)
        } else if (needsRebuild(abspath, event)) {
          scheduleRebuild()
        }
      })
    } catch (err) {
      log(`can't watch ${dir}: ${err.message}`)
    }
  }
}

// --- HTTP -------------------------------------------------------------------------------

const TYPES = {
  '.html': 'text/html; charset=utf-8', '.css': 'text/css', '.js': 'text/javascript', '.json': 'application/json',
  '.svg': 'image/svg+xml', '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg', '.gif': 'image/gif',
  '.webp': 'image/webp', '.ico': 'image/x-icon', '.woff': 'font/woff', '.woff2': 'font/woff2', '.xml': 'application/xml',
  '.txt': 'text/plain', '.pdf': 'application/pdf',
}

const CLIENT = `(() => {
  const page = decodeURIComponent(location.pathname.slice(1)) || 'index.html'
  let es
  function connect () {
    es = new EventSource('/__preview/events?page=' + encodeURIComponent(page))
    es.addEventListener('page', (e) => {
      const { html, nav } = JSON.parse(e.data)
      update(html, nav)
    })
    es.addEventListener('reload', () => location.reload())
    es.addEventListener('status', onStatus)
    es.addEventListener('cursor', onCursor)
    es.addEventListener('error', () => setTimeout(check, 1000))
  }
  // A dropped stream doesn't mean the server is gone (navigation, sleep, bfcache): ask it.
  async function check () {
    if (es.readyState === EventSource.OPEN) return
    try {
      await fetch('/__preview/ping', { cache: 'no-store' })
      if (es.readyState === EventSource.CLOSED) connect()
    } catch {
      show('The preview server stopped. Open the preview again from the editor.', true)
    }
  }
  // A page restored from the back/forward cache missed updates while it was away. Safari
  // reconnects its stream (even a closed one, so don't open another); catch up on the content.
  addEventListener('pageshow', async (e) => {
    if (!e.persisted) return
    if (es.readyState === EventSource.CLOSED) connect()
    update(await (await fetch(location.href, { cache: 'no-store' })).text(), true)
  })
  const swap = (selector, next) => {
    const a = document.querySelector(selector), b = next.querySelector(selector)
    if (a && b) a.replaceWith(b)
    return a && b
  }
  function update (html, nav) {
    const next = new DOMParser().parseFromString(html, 'text/html')
    if (!swap('article.doc', next)) return location.reload()
    if (nav) swap('.nav-menu', next)
    document.title = next.title
    document.querySelectorAll('article.doc pre.highlight code').forEach((el) => window.hljs?.highlightElement?.(el))
  }
  let badge
  const show = (text, failed) => {
    if (!text) return badge?.remove()
    if (!badge?.isConnected) {
      badge = document.body.appendChild(document.createElement('div'))
      badge.style.cssText = 'position:fixed;right:12px;bottom:12px;z-index:9999;max-width:40em;padding:6px 10px;' +
        'border-radius:6px;font:13px system-ui,sans-serif;color:#fff;box-shadow:0 1px 4px #0004;cursor:pointer'
      badge.title = 'Click to hide'
      badge.onclick = () => badge.remove()
    }
    badge.style.background = failed ? '#b00020' : '#333'
    badge.textContent = text
  }
  function onStatus (e) {
    const { state, phase, error } = JSON.parse(e.data)
    show(state === 'ready' ? '' : state === 'failed' ? 'Rebuild failed: ' + error : 'Rebuilding site: ' + (phase || '') + '…', state === 'failed')
  }
  function onCursor (e) {
    const { page: target, line } = JSON.parse(e.data)
    if (target !== page) return (location.href = '/' + target + '#preview-line-' + line)
    scrollToLine(line)
  }
  function scrollToLine (line) {
    let best, bestLine = -1
    document.querySelectorAll('[class*="preview-line-"]').forEach((el) => {
      const n = +(el.className.match(/preview-line-(\\d+)/) || [])[1]
      if (n <= line && n > bestLine) { best = el; bestLine = n }
    })
    if (!best) return
    const r = best.getBoundingClientRect()
    if (r.top < 80 || r.bottom > innerHeight) best.scrollIntoView({ block: 'center', behavior: 'smooth' })
  }
  // Double-click a block to show its source line in the editor.
  addEventListener('dblclick', (e) => {
    const el = e.target.closest?.('[class*="preview-line-"]')
    const line = el && +(el.className.match(/preview-line-(\\d+)/) || [])[1]
    if (line) fetch('/__preview/reveal', { method: 'POST', body: JSON.stringify({ page, line }) })
  })
  const m = location.hash.match(/^#preview-line-(\\d+)$/)
  if (m) addEventListener('load', () => scrollToLine(+m[1]))
  connect()
})()`

/** Shown until the first build is done; reloads into the requested page then. */
const LOADING = `<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>Building preview…</title>
<style>
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; font: 15px system-ui, sans-serif;
    color: #333; background: #fafafa }
  @media (prefers-color-scheme: dark) { body { color: #ddd; background: #1e1e1e } }
  main { text-align: center; padding: 16px }
  .spin { width: 28px; height: 28px; margin: 0 auto 16px; border: 3px solid #8884; border-top-color: currentColor;
    border-radius: 50%; animation: spin 1s linear infinite }
  @keyframes spin { to { transform: rotate(360deg) } }
  #time { margin-top: 4px; font-size: 13px; opacity: .6 }
  pre { max-width: 80ch; white-space: pre-wrap; text-align: left; color: #d33 }
</style></head>
<body><main>
  <div class="spin" id="spin"></div>
  <div id="msg">Building the site…</div>
  <div id="time"></div>
  <pre id="err" hidden></pre>
</main>
<script>
  const es = new EventSource('/__preview/events')
  let started = Date.now()
  es.addEventListener('status', (e) => {
    const s = JSON.parse(e.data)
    if (s.state === 'ready') return location.reload()
    if (s.started) started = s.started
    const failed = s.state === 'failed'
    spin.hidden = time.hidden = failed
    err.hidden = !failed
    err.textContent = s.error || ''
    msg.textContent = failed ? 'The site build failed. Save a file to retry.' : 'Building the site: ' + (s.phase || '') + '…'
  })
  es.addEventListener('reload', () => location.reload())
  es.addEventListener('error', () => setTimeout(async () => {
    if (es.readyState === EventSource.OPEN) return
    try {
      await fetch('/__preview/ping', { cache: 'no-store' })
      if (es.readyState === EventSource.CLOSED) location.reload()
    } catch {
      spin.hidden = time.hidden = true
      msg.textContent = 'The preview server stopped. Open the preview again from the editor.'
    }
  }, 1000))
  addEventListener('pageshow', (e) => e.persisted && location.reload())
  setInterval(() => (time.textContent = Math.round((Date.now() - started) / 1000) + ' s'), 1000)
</script>
</body></html>`

/** Serve a request; one that fails gets a 500 and doesn't take the server down. */
async function handle (req, res) {
  try {
    await serve(req, res)
  } catch (err) {
    log(`${req.method} ${req.url} failed: ${err.stack || err}`)
    if (!res.headersSent) reply(res, 500, 'text/plain', String(err))
    else res.destroy()
  }
}

async function serve (req, res) {
  const url = new URL(req.url, 'http://localhost')
  if (url.pathname.startsWith('/__preview/') && url.pathname !== '/__preview/open') return api(req, res, url)
  if (req.method !== 'GET' && req.method !== 'HEAD') return reply(res, 405, 'text/plain', 'method not allowed')
  if (!S.site) return reply(res, 200, TYPES['.html'], LOADING)
  if (url.pathname === '/__preview/open') return open(res, url)

  const site = S.site
  const file = findRoute(decodeURIComponent(url.pathname))
  if (!file) {
    const notFound = site.routes.get('404.html')
    return reply(res, 404, TYPES['.html'], notFound ? inject(notFound.contents.toString()) : 'not found')
  }
  if (site.dirty.has(file)) renderPage(file)
  const type = TYPES[path.extname(file.out.path).toLowerCase()] || 'application/octet-stream'
  const contents = await contentsOf(file)
  reply(res, 200, type, type.startsWith('text/html') ? inject(contents.toString()) : contents)
}

/** The site file served at a URL path. */
function findRoute (pathname) {
  let key = pathname.replace(/^\/+/, '')
  if (key === '' || key.endsWith('/')) key += 'index.html'
  const routes = S.site.routes
  return routes.get(key) || routes.get(key + '.html') || routes.get(key + '/index.html')
}

/** A file's contents as a Buffer. Some, like assets that extensions add to the UI, are a
 * stream until published; read it once and keep the result. */
async function contentsOf (file) {
  const contents = file.contents
  if (contents == null) return Buffer.alloc(0)
  if (Buffer.isBuffer(contents) || typeof contents === 'string') return contents
  const chunks = []
  for await (const chunk of contents) chunks.push(Buffer.from(chunk))
  return (file.contents = Buffer.concat(chunks))
}

const inject = (html) => html.replace(/<\/body>/i, '<script src="/__preview/client.js"></script></body>')

function reply (res, status, type, body) {
  res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store' })
  res.end(body)
}

function api (req, res, url) {
  const route = url.pathname.slice('/__preview/'.length)
  if (route === 'client.js') return reply(res, 200, TYPES['.js'], CLIENT)
  if (route === 'ping') return reply(res, 204, 'text/plain', '')
  if (route === 'events') {
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' })
    res.write(': connected\n\n')
    const client = { res, page: url.searchParams.get('page') || 'index.html' }
    S.clients.add(client)
    send(client, 'status', S.status)
    req.on('close', () => S.clients.delete(client))
    return
  }
  if (req.method !== 'POST') return reply(res, 404, 'text/plain', 'not found')
  if (route === 'reveal') return reveal(req, res)
  if (req.headers['x-preview-token'] !== S.token) return reply(res, 403, 'text/plain', 'forbidden')
  let body = ''
  req.on('data', (chunk) => (body += chunk))
  req.on('end', () => {
    try {
      const msg = JSON.parse(body || '{}')
      if (route === 'update') update(msg.path, msg.text ?? null)
      else if (route === 'cursor') cursor(msg.path, msg.line)
      else return reply(res, 404, 'text/plain', 'not found')
      reply(res, 204, 'text/plain', '')
    } catch (err) {
      log('request failed: ' + (err.stack || err))
      reply(res, 500, 'text/plain', String(err))
    }
  })
}

/** Tell the editor to show the source line of a block double-clicked in a preview page. Browsers
 * send these, so they can't carry the token; only the preview's own pages may. */
function reveal (req, res) {
  const sameOrigin = req.headers.origin ? req.headers.origin === S.base : req.headers['sec-fetch-site'] === 'same-origin'
  if (!sameOrigin) return reply(res, 403, 'text/plain', 'forbidden')
  let body = ''
  req.on('data', (chunk) => (body += chunk))
  req.on('end', () => {
    try {
      const { page, line } = JSON.parse(body || '{}')
      const file = S.site && typeof page === 'string' && findRoute(page)
      if (!file || !isPage(file) || !file.src.abspath || !Number.isInteger(line)) return reply(res, 404, 'text/plain', 'not found')
      process.stdout.write(`ANTORA_PREVIEW_REVEAL ${JSON.stringify({ path: realpath(file.src.abspath), line })}\n`)
      reply(res, 204, 'text/plain', '')
    } catch (err) {
      reply(res, 400, 'text/plain', String(err))
    }
  })
}

/** Redirect to the page for a source file, or the site start page. */
function open (res, url) {
  const file = S.site.byPath.get(realpath(url.searchParams.get('path') || '.'))
  res.writeHead(302, { location: file && isPage(file) ? '/' + file.out.path : '/' })
  res.end()
}

function send (client, event, data) {
  client.res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`)
}

function broadcast (event, data) {
  for (const c of S.clients) send(c, event, data)
}

function log (msg) {
  process.stderr.write(`[antora-preview] ${msg}\n`)
}

// --- Main -------------------------------------------------------------------------------

async function main () {
  const args = process.argv.slice(2)
  const portArg = args.indexOf('--port')
  const port = portArg >= 0 ? Number(args.splice(portArg, 2)[1]) : 0
  if (!args[0]) {
    log('usage: antora-preview.js <playbook> [--port N]')
    process.exit(2)
  }
  S.playbookFile = path.resolve(args[0])
  S.req = antoraRequire(S.playbookFile)
  S.token = crypto.randomBytes(16).toString('hex')
  process.stdout.on('error', () => {}) // antora-ls stopped reading; it closes stdin when it exits
  // A bug in one request or update shouldn't end the preview.
  process.on('uncaughtException', (err) => log('internal error: ' + (err.stack || err)))
  process.on('unhandledRejection', (err) => log('internal error: ' + (err?.stack || err)))
  const server = http.createServer(handle)
  await new Promise((resolve) => server.listen(port, '127.0.0.1', resolve))
  S.base = `http://127.0.0.1:${server.address().port}`
  process.stdout.write(`ANTORA_PREVIEW ${S.base} ${S.token}\n`)
  runBuild()
  // Keep SSE connections alive through proxies and sleeping laptops.
  setInterval(() => S.clients.forEach((c) => c.res.write(': ping\n\n')), 15000).unref()
  // Exit with the editor: antora-ls holds our stdin open.
  process.stdin.on('end', () => process.exit(0))
  process.stdin.resume()
}

module.exports = { register }
if (require.main === module) {
  main().catch((err) => {
    log(err.stack || String(err))
    process.exit(1)
  })
}
