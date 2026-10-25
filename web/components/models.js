/* ──────────────────────────────────────────────────────────────
   models.js — on-demand AI model state (CLAP, AST)

   The core app works with zero models. Gated features (semantic /
   mood search, SFX counts, radar generate, auto-tag suggestions)
   check this store and show an inline "Needs the X model — Download"
   affordance instead of failing.

   Contract (backend):
     GET  /api/models                  → [{name,label,total_bytes,status,downloading}]
     POST /api/models/:name/download   → 202 + status object (idempotent)
     GET  /api/models/events           → SSE of the same status objects
     409 {"error":"model_not_ready","model":"clap"} from gated endpoints

   Registered as Alpine.store('models') for the panel, and exposed as
   window.DaouchiModels for the vanilla components.
   ────────────────────────────────────────────────────────────── */
;(function () {
  'use strict'

  // URL fragment → models the endpoint needs (used to detect long first-inference)
  const GATED_URLS = [
    ['/api/sounds/semantic_search', ['clap']],
    ['/api/ai/mood_search',         ['clap']],
    ['/api/ai/sfx_',                ['clap']],
    ['/api/radar/generate',         ['clap']],
    ['/suggested_tags',             ['ast', 'clap']],
  ]
  const OPTIMIZING_AFTER_MS = 4000   // a gated call this slow on a ready model = first CoreML compile

  const esc = (s) => String(s == null ? '' : s).replace(/[&<>"']/g,
    c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c]))

  function fmtBytes(n) {
    n = Number(n) || 0
    if (n >= 1073741824) return (n / 1073741824).toFixed(1) + ' GB'
    if (n >= 1048576)    return Math.round(n / 1048576) + ' MB'
    if (n >= 1024)       return Math.round(n / 1024) + ' KB'
    return n + ' B'
  }

  let _es = null
  let _sseOk = false
  let _pollTimer = null

  const api = {
    list: [],            // status objects from the backend
    loaded: false,       // first fetch finished
    available: true,     // false when /api/models 404s (backend not built yet)
    open: false,         // panel visibility
    optimizing: {},      // name → true while the first inference compiles
    errors: {},          // name → last download error text

    /* ── Lookup ─────────────────────────────────────────────── */
    get(name) { return this.list.find(m => m.name === name) || null },

    // A model is "missing" only once we positively know it is not ready.
    // Unknown state (not loaded / endpoint absent) never blocks a feature.
    isMissing(name) {
      if (!this.loaded || !this.available) return false
      const m = this.get(name)
      return !!m && m.status !== 'ready'
    },

    // names: string | string[] → names currently not ready
    missing(names) {
      const arr = Array.isArray(names) ? names : [names]
      return arr.filter(n => this.isMissing(n))
    },

    anyMissing() { return this.missing(['clap', 'ast']).length > 0 },
    anyDownloading() { return this.list.some(m => m.downloading) },
    label(name) { const m = this.get(name); return m ? m.label : name.toUpperCase() },
    fmt(n) { return fmtBytes(n) },

    pct(m) {
      const d = m && m.downloading
      if (!d || !d.total_bytes) return 0
      return Math.min(100, Math.round(d.done_bytes / d.total_bytes * 100))
    },

    sizeOf(names) {
      return this.missing(names).reduce((s, n) => s + ((this.get(n) || {}).total_bytes || 0), 0)
    },

    /* ── Loading ────────────────────────────────────────────── */
    async refresh() {
      try {
        const r = await _rawFetch('/api/models')
        if (r.status === 404) { this.available = false; this.loaded = true; _changed(); return }
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        this.list = Array.isArray(data) ? data : (data.models || [])
        this.available = true
        this.loaded = true
      } catch (_) {
        // Server unreachable or malformed — leave state as is; features stay unblocked
        this.loaded = true
      }
      this._schedulePoll()
      _changed()
    },

    _apply(st) {
      if (!st || !st.name) return
      const i = this.list.findIndex(m => m.name === st.name)
      if (i >= 0) this.list.splice(i, 1, { ...this.list[i], ...st })
      else this.list.push(st)
      if (st.status === 'ready') {
        delete this.errors[st.name]
        // Leave the "optimizing" flag alone: the first inference clears it
      }
      this._schedulePoll()
      _changed()
    },

    connect() {
      if (_es || typeof EventSource === 'undefined') return
      try {
        _es = new EventSource('/api/models/events')
      } catch (_) { _es = null; return }
      _es.onopen = () => { _sseOk = true }
      _es.onmessage = (ev) => {
        _sseOk = true
        try {
          const d = JSON.parse(ev.data)
          if (Array.isArray(d)) d.forEach(x => this._apply(x)); else this._apply(d)
        } catch (_) {}
      }
      _es.onerror = () => {
        _sseOk = false
        // CLOSED = endpoint 404/refused permanently; fall back to polling
        if (_es && _es.readyState === 2) { _es = null }
        this._schedulePoll()
      }
    },

    // Polling fallback: only while a download is running and SSE is not delivering
    _schedulePoll() {
      clearTimeout(_pollTimer)
      _pollTimer = null
      if (!this.available || !this.anyDownloading() || _sseOk) return
      _pollTimer = setTimeout(() => this.refresh(), 1500)
    },

    /* ── Download ───────────────────────────────────────────── */
    // names: string | string[]
    async download(names) {
      const arr = Array.isArray(names) ? names : [names]
      for (const name of arr) {
        const m = this.get(name)
        if (m && m.status === 'ready') continue
        delete this.errors[name]
        // Optimistic: show the bar immediately; SSE/poll replaces it
        if (m && !m.downloading) {
          this._apply({ name, downloading: { done_bytes: 0, total_bytes: m.total_bytes, file: '' } })
        }
        try {
          const r = await _rawFetch(`/api/models/${encodeURIComponent(name)}/download`, { method: 'POST' })
          if (!r.ok) throw new Error(r.status === 404 ? 'Download is not available in this build' : (r.statusText || 'HTTP ' + r.status))
          const st = await r.json().catch(() => null)
          if (st && st.name) this._apply(st)
        } catch (err) {
          this.errors[name] = err.message || 'Download failed'
          this._apply({ name, downloading: null })
        }
      }
      this.connect()
      _changed()
    },

    /* ── Global 409 / slow-call handling ────────────────────── */
    onNotReady(model) {
      this.refresh().then(() => {
        const m = this.get(model)
        if (m && m.status === 'ready') {
          // Files are there but the backend still said no — engine is warming up
          this.setOptimizing(model, true)
          setTimeout(() => this.setOptimizing(model, false), 20000)
        }
      })
    },

    setOptimizing(name, on) {
      if (!!this.optimizing[name] === on) return
      if (on) this.optimizing[name] = true; else delete this.optimizing[name]
      _changed()
    },

    isOptimizing(names) {
      const arr = Array.isArray(names) ? names : [names]
      return arr.some(n => this.optimizing[n])
    },

    /* ── Panel ──────────────────────────────────────────────── */
    openPanel() { this.open = true; this.refresh() },
    closePanel() { this.open = false },
  }

  /* ── Wiring ──────────────────────────────────────────────────── */
  const _rawFetch = window.fetch.bind(window)

  function store() {
    return (window.Alpine && window.Alpine.store && window.Alpine.store('models')) || api
  }

  function _changed() {
    renderGates()
    document.dispatchEvent(new CustomEvent('models-changed'))
  }

  // Global fetch helper: understands the 409 model_not_ready shape and flags
  // slow gated calls on a ready model as "Optimizing for this Mac…".
  window.fetch = async function (input, init) {
    const url = typeof input === 'string' ? input : (input && input.url) || ''
    const gated = GATED_URLS.find(([frag]) => url.indexOf(frag) !== -1)
    let timer = null
    if (gated) {
      timer = setTimeout(() => {
        const s = store()
        gated[1].forEach(n => { if (s.get(n) && s.get(n).status === 'ready') s.setOptimizing(n, true) })
      }, OPTIMIZING_AFTER_MS)
    }
    try {
      const res = await _rawFetch(input, init)
      if (res.status === 409) {
        res.clone().json().then(body => {
          if (body && body.error === 'model_not_ready') store().onNotReady(body.model)
        }).catch(() => {})
      }
      return res
    } finally {
      if (timer) {
        clearTimeout(timer)
        const s = store()
        gated[1].forEach(n => s.setOptimizing(n, false))
      }
    }
  }

  /* ── Inline gate affordance ──────────────────────────────────────
     <div data-model-gate="clap"></div>  or  data-model-gate="ast,clap"
     Rendered by renderGates(): hidden when nothing is missing. */
  function gateHtml(names) {
    const s = store()
    const miss = s.missing(names)
    if (!miss.length) {
      if (s.isOptimizing(names)) {
        return '<span class="spinner" style="width:9px;height:9px;border-width:1.5px;margin:0 6px 0 0;"></span>' +
               'Optimizing for this Mac… first run can take a couple of minutes.'
      }
      return ''
    }
    const labels = miss.map(n => esc(s.label(n))).join(' + ')
    const noun = miss.length > 1 ? 'models' : 'model'
    const size = fmtBytes(s.sizeOf(miss))
    const busy = miss.some(n => (s.get(n) || {}).downloading)
    const err = miss.map(n => s.errors[n]).find(Boolean)
    let html = `<span>Needs the ${labels} ${noun} (≈${esc(size)})</span> `
    if (busy) {
      const m = s.get(miss.find(n => (s.get(n) || {}).downloading))
      html += `<span class="model-gate-bar"><span style="width:${s.pct(m)}%"></span></span> <span>${s.pct(m)}%</span>`
    } else {
      const arg = esc(JSON.stringify(miss))
      html += `<button class="ctrl-btn" style="font-size:10px;padding:2px 8px;" onclick="DaouchiModels.download(${arg})">Download</button>`
    }
    html += ` <a href="#" class="model-gate-link" onclick="DaouchiModels.openPanel();return false;">Details</a>`
    if (err) html += `<div class="model-gate-err">${esc(err)}</div>`
    return html
  }

  function renderGates(root) {
    (root || document).querySelectorAll('[data-model-gate]').forEach(el => {
      const names = el.getAttribute('data-model-gate').split(',').map(x => x.trim()).filter(Boolean)
      const html = gateHtml(names)
      el.classList.add('model-gate')
      el.style.display = html ? '' : 'none'
      if (el._gateHtml !== html) { el.innerHTML = html; el._gateHtml = html }
    })
  }

  // Thin facade for vanilla callers — always routes through the reactive store
  window.DaouchiModels = {
    isMissing: n => store().isMissing(n),
    missing: n => store().missing(n),
    download: n => store().download(n),
    openPanel: () => store().openPanel(),
    refresh: () => store().refresh(),
    gateHtml,
    renderGates,
    // For pages without Alpine (bootstrap.html)
    initStandalone() { api.refresh().then(() => api.connect()) },
    _store: store,
  }

  function start() {
    const s = store()
    s.refresh().then(() => s.connect())
  }

  document.addEventListener('alpine:init', () => {
    window.Alpine.store('models', api)
    start()
  })
  document.addEventListener('DOMContentLoaded', () => renderGates())
})()
