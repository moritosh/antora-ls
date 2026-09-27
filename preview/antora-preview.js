'use strict'

// Live preview for Antora sites.
//
// Builds the site once from HEAD (the working tree of local content sources) using the
// project's own Antora, keeps the result in memory and serves it over HTTP. When a file
// changes — saved to disk, or unsaved text posted by antora-ls — only the affected page is
// re-rendered and pushed to open browsers, which swap in the new article without a reload.
//
// Usage: bun antora-preview.js <playbook> [--port N]
// Prints `ANTORA_PREVIEW <url> <token>` on stdout once the server is ready; logs go to stderr.
// Only Node built-ins are used, so it runs under Bun or Node.

const fs = require('node:fs')
const http = require('node:http')
const path = require('node:path')
const crypto = require('node:crypto')
const { createRequire } = require('node:module')

// Shared with the copy of this module that Antora loads as an extension.
const S = (globalThis.__antoraPreview ??= { clients: new Set(), overlays: new Map() })

// --- Antora extension -------------------------------------------------------------------

/** Captures the build's catalogs instead of publishing files. Registered by build(). */
function register () {
  const out = S.pending
  if (!out) return // loaded outside the preview server

  this.once('contentClassified', ({ contentCatalog }) => {
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

  this.once('beforePublish', ({ playbook, contentCatalog, uiCatalog, siteCatalog, siteAsciiDocConfig }) => {
    Object.assign(out, { playbook, contentCatalog, siteAsciiDocConfig, fxns: this.getFunctions() })
    out.composePage = out.fxns.createPageComposer(playbook, contentCatalog, uiCatalog, playbook.env).composePage
    out.routes = new Map()
    for (const catalog of [contentCatalog, uiCatalog, siteCatalog]) {
      for (const file of catalog.getFiles()) if (file.out) out.routes.set(file.out.path, file)
    }
    out.dirty = new Set()
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
  const playbook = req('@antora/playbook-builder')(['--playbook', S.playbookFile], process.env)
  // stdout carries the handshake line; Antora's log goes to stderr, readable for the editor's log.
  playbook.runtime.log = Object.assign({}, playbook.runtime.log, { format: 'pretty', destination: { file: 'stderr' } })
  try {
    req('@antora/logger').configureLogger(playbook.runtime.log, path.dirname(S.playbookFile))
  } catch {}
  useHead(playbook)
  playbook.antora ??= {}
  playbook.antora.extensions = [...(playbook.antora.extensions || []), { require: __filename }]
  playbook.runtime.quiet = true

  const out = (S.pending = {})
  const started = Date.now()
  try {
    await req('@antora/site-generator')(playbook)
  } finally {
    S.pending = undefined
  }
  if (!out.routes) throw new Error('site generation stopped before pages were composed')
  S.site = out
  S.localDirs = playbook.content.sources.filter((s) => isLocal(s.url)).flatMap((s) => {
    const dir = path.resolve(playbook.dir || '.', s.url)
    return [].concat(s.startPaths ?? s.startPath ?? '').map((p) => path.join(dir, p.split(/[*{]/)[0]))
  })
  log(`built ${[...out.routes.keys()].filter((p) => p.endsWith('.html')).length} pages in ${Date.now() - started} ms`)
}

let rebuildTimer
function scheduleRebuild () {
  clearTimeout(rebuildTimer)
  rebuildTimer = setTimeout(async () => {
    try {
      await build()
      broadcast('reload', {})
      watch()
    } catch (err) {
      log('rebuild failed: ' + (err.stack || err))
    }
  }, 150)
}

// --- Incremental updates ------------------------------------------------------------------

function renderPage (page) {
  const site = S.site
  page.contents = site.sources.get(page)
  page.mediaType = 'text/asciidoc'
  delete page.asciidoc
  const cv = site.contentCatalog.getComponentVersion(page.src.component, page.src.version)
  site.fxns.convertDocument(page, site.contentCatalog, cv?.asciidoc || site.siteAsciiDocConfig)
  site.composePage(page, site.contentCatalog, site.navigationCatalog)
  site.dirty.delete(page)
}

const isPage = (file) => file.src.family === 'page' && file.out && file.src.mediaType === 'text/asciidoc'

/** Apply new contents for a file (text = null means: read it from disk). */
function update (abspath, text) {
  abspath = realpath(abspath)
  if (text == null) S.overlays.delete(abspath)
  else S.overlays.set(abspath, text)

  const site = S.site
  const file = site?.byPath.get(abspath)
  if (!file) {
    // New files, antora.yml, UI changes, ...: anything outside the catalog needs a full build.
    if (S.localDirs?.some((d) => abspath.startsWith(d)) || path.basename(abspath) === 'antora.yml') scheduleRebuild()
    return
  }
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

let watchers = []
function watch () {
  watchers.forEach((w) => w.close())
  watchers = []
  const dirs = new Set(S.localDirs)
  const supplemental = S.site.playbook.ui?.supplementalFiles
  if (typeof supplemental === 'string') dirs.add(path.resolve(S.site.playbook.dir || '.', supplemental))
  for (const dir of dirs) {
    try {
      watchers.push(
        fs.watch(dir, { recursive: true }, (event, name) => {
          if (!name || name.split(path.sep).some((s) => s.startsWith('.'))) return
          const abspath = realpath(path.join(dir, name))
          if (event === 'rename' || !S.site.byPath.has(abspath)) return scheduleRebuild()
          if (!S.overlays.has(abspath)) update(abspath, null)
        })
      )
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
  const es = new EventSource('/__preview/events?page=' + encodeURIComponent(page))
  const swap = (selector, next) => {
    const a = document.querySelector(selector), b = next.querySelector(selector)
    if (a && b) a.replaceWith(b)
    return a && b
  }
  es.addEventListener('page', (e) => {
    const { html, nav } = JSON.parse(e.data)
    const next = new DOMParser().parseFromString(html, 'text/html')
    if (!swap('article.doc', next)) return location.reload()
    if (nav) swap('.nav-menu', next)
    document.title = next.title
    document.querySelectorAll('article.doc pre.highlight code').forEach((el) => window.hljs?.highlightElement?.(el))
  })
  es.addEventListener('reload', () => location.reload())
  es.addEventListener('cursor', (e) => {
    const { page: target, line } = JSON.parse(e.data)
    if (target !== page) return (location.href = '/' + target + '#preview-line-' + line)
    scrollToLine(line)
  })
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
  const m = location.hash.match(/^#preview-line-(\\d+)$/)
  if (m) addEventListener('load', () => scrollToLine(+m[1]))
})()`

function handle (req, res) {
  const url = new URL(req.url, 'http://localhost')
  if (url.pathname.startsWith('/__preview/')) return api(req, res, url)
  if (req.method !== 'GET' && req.method !== 'HEAD') return reply(res, 405, 'text/plain', 'method not allowed')

  let key = decodeURIComponent(url.pathname).replace(/^\/+/, '')
  if (key === '' || key.endsWith('/')) key += 'index.html'
  const site = S.site
  const file = site.routes.get(key) || site.routes.get(key + '.html') || site.routes.get(key + '/index.html')
  if (!file) {
    const notFound = site.routes.get('404.html')
    return reply(res, 404, TYPES['.html'], notFound ? inject(notFound.contents.toString()) : 'not found')
  }
  if (site.dirty.has(file)) renderPage(file)
  const type = TYPES[path.extname(file.out.path).toLowerCase()] || 'application/octet-stream'
  const body = type.startsWith('text/html') ? inject(file.contents.toString()) : file.contents
  reply(res, 200, type, body)
}

const inject = (html) => html.replace(/<\/body>/i, '<script src="/__preview/client.js"></script></body>')

function reply (res, status, type, body) {
  res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store' })
  res.end(body)
}

function api (req, res, url) {
  const route = url.pathname.slice('/__preview/'.length)
  if (route === 'client.js') return reply(res, 200, TYPES['.js'], CLIENT)
  if (route === 'events') {
    res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store', connection: 'keep-alive' })
    res.write(': connected\n\n')
    const client = { res, page: url.searchParams.get('page') || 'index.html' }
    S.clients.add(client)
    req.on('close', () => S.clients.delete(client))
    return
  }
  if (route === 'open') {
    // Redirect to the page for a source file, or the site start page.
    const file = S.site.byPath.get(realpath(url.searchParams.get('path') || '.'))
    const target = file && isPage(file) ? '/' + file.out.path : '/'
    res.writeHead(302, { location: target })
    return res.end()
  }
  if (req.method !== 'POST') return reply(res, 404, 'text/plain', 'not found')
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
  await build()
  watch()
  const server = http.createServer(handle)
  server.listen(port, '127.0.0.1', () => {
    process.stdout.write(`ANTORA_PREVIEW http://127.0.0.1:${server.address().port} ${S.token}\n`)
  })
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
