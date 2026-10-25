;(function () {
  'use strict'

  // ═══════════════════════════════════════════════════════════════
  // Sonic Horizon Dashboard — Emotion Matrix + Distribution Blueprint
  // ═══════════════════════════════════════════════════════════════

  let moodCanvas = null
  let moodCtx = null
  let moodDragging = false
  let moodX = 0.0   // valence  -1..1
  let moodY = 0.0   // arousal  -1..1
  let moodDebounceTimer = null
  let moodResults = []
  let moodQuery = ''
  let moodLoading = false

  // ── Proportional Bar state ───────────────────────────────────────
  let sfxCategories = []
  let sfxCategoryCounts = {}
  let distribStats = { total: 0, sfx: 0, music: 0, stems: 0 }
  let distribLoading = false
  let distribTimer = null

  // ── Dashboard initialized flag ───────────────────────────────────
  let dashboardInitialized = false

  // ═══════════════════════════════════════════════════════════════
  // Public API — called from index.html when tab becomes active
  // ═══════════════════════════════════════════════════════════════
  // CLAP finished downloading while the dashboard is open — fill in the counts
  let _clapWasMissing = false
  document.addEventListener('models-changed', () => {
    if (!dashboardInitialized || !window.DaouchiModels) return
    const missing = DaouchiModels.isMissing('clap')
    if (_clapWasMissing && !missing) loadSfxCategoryCounts()
    _clapWasMissing = missing
  })

  window.dashboardInit = function () {
    if (dashboardInitialized) {
      // Re-draw canvas (tab might have been hidden)
      requestAnimationFrame(renderMoodWheel)
      return
    }
    dashboardInitialized = true
    initMoodWheel()
    loadSfxCategories()
    loadDistribStats()
  }

  // ═══════════════════════════════════════════════════════════════
  // Emotion Matrix — Russell Circumplex Wheel
  // ═══════════════════════════════════════════════════════════════

  function initMoodWheel () {
    moodCanvas = document.getElementById('mood-canvas')
    if (!moodCanvas) return
    moodCtx = moodCanvas.getContext('2d')
    resizeMoodCanvas()
    window.addEventListener('resize', resizeMoodCanvas)
    renderMoodWheel()
    attachMoodEvents()
  }

  function resizeMoodCanvas () {
    if (!moodCanvas) return
    const rect = moodCanvas.parentElement.getBoundingClientRect()
    const size = Math.min(rect.width, 340)
    moodCanvas.width  = size * window.devicePixelRatio
    moodCanvas.height = size * window.devicePixelRatio
    moodCanvas.style.width  = size + 'px'
    moodCanvas.style.height = size + 'px'
    renderMoodWheel()
  }

  function renderMoodWheel () {
    if (!moodCtx || !moodCanvas) return
    const W = moodCanvas.width
    const H = moodCanvas.height
    const cx = W / 2
    const cy = H / 2
    const R = Math.min(cx, cy) - 8 * window.devicePixelRatio
    const dpr = window.devicePixelRatio

    moodCtx.clearRect(0, 0, W, H)

    // ── Background quadrant fills ────────────────────────────────
    const quadrants = [
      // [startAngle, endAngle, color, label, sublabel]
      [Math.PI, Math.PI * 1.5, 'rgba(155,109,255,0.15)', 'TENSE', 'Suspenseful'],           // top-left
      [Math.PI * 1.5, Math.PI * 2, 'rgba(228,122,42,0.15)', 'AGGRESSIVE', 'Energetic'],      // top-right
      [Math.PI * 0.5, Math.PI, 'rgba(77,142,240,0.15)', 'MELANCHOLY', 'Sad / Dark'],         // bottom-left
      [0, Math.PI * 0.5, 'rgba(61,186,110,0.15)', 'CALM', 'Peaceful / Ambient'],             // bottom-right
    ]

    quadrants.forEach(([sa, ea, fill]) => {
      moodCtx.beginPath()
      moodCtx.moveTo(cx, cy)
      moodCtx.arc(cx, cy, R, sa, ea)
      moodCtx.closePath()
      moodCtx.fillStyle = fill
      moodCtx.fill()
    })

    // ── Concentric reference circles ─────────────────────────────
    for (let i = 1; i <= 3; i++) {
      moodCtx.beginPath()
      moodCtx.arc(cx, cy, (R / 3) * i, 0, Math.PI * 2)
      moodCtx.strokeStyle = 'rgba(255,255,255,0.06)'
      moodCtx.lineWidth = 1
      moodCtx.stroke()
    }

    // ── Axis lines ───────────────────────────────────────────────
    moodCtx.strokeStyle = 'rgba(255,255,255,0.15)'
    moodCtx.lineWidth = 1
    moodCtx.setLineDash([4, 4])
    moodCtx.beginPath(); moodCtx.moveTo(cx - R, cy); moodCtx.lineTo(cx + R, cy); moodCtx.stroke()
    moodCtx.beginPath(); moodCtx.moveTo(cx, cy - R); moodCtx.lineTo(cx, cy + R); moodCtx.stroke()
    moodCtx.setLineDash([])

    // ── Outer ring ───────────────────────────────────────────────
    moodCtx.beginPath()
    moodCtx.arc(cx, cy, R, 0, Math.PI * 2)
    moodCtx.strokeStyle = 'rgba(255,255,255,0.2)'
    moodCtx.lineWidth = 1.5
    moodCtx.stroke()

    // ── Quadrant labels ──────────────────────────────────────────
    const fs = 9 * dpr
    moodCtx.font = `600 ${fs}px system-ui, sans-serif`
    moodCtx.textAlign = 'center'
    moodCtx.fillStyle = 'rgba(255,255,255,0.4)'
    const pad = 22 * dpr
    moodCtx.fillText('TENSE',       cx - R * 0.52, cy - R * 0.52)
    moodCtx.fillText('ENERGETIC',   cx + R * 0.52, cy - R * 0.52)
    moodCtx.fillText('MELANCHOLY',  cx - R * 0.52, cy + R * 0.52)
    moodCtx.fillText('CALM',        cx + R * 0.52, cy + R * 0.52)

    // ── Axis labels ──────────────────────────────────────────────
    moodCtx.font = `500 ${8 * dpr}px system-ui, sans-serif`
    moodCtx.fillStyle = 'rgba(255,255,255,0.25)'
    moodCtx.fillText('HIGH AROUSAL', cx, cy - R - 4 * dpr)
    moodCtx.fillText('LOW AROUSAL',  cx, cy + R + 12 * dpr)
    moodCtx.save()
    moodCtx.translate(cx - R - 4 * dpr, cy)
    moodCtx.rotate(-Math.PI / 2)
    moodCtx.fillText('NEGATIVE', 0, 0)
    moodCtx.restore()
    moodCtx.save()
    moodCtx.translate(cx + R + 4 * dpr, cy)
    moodCtx.rotate(Math.PI / 2)
    moodCtx.fillText('POSITIVE', 0, 0)
    moodCtx.restore()

    // ── Cursor dot ───────────────────────────────────────────────
    // moodX = valence (x), moodY = arousal (y, inverted because canvas y grows down)
    const dotX = cx + moodX * R
    const dotY = cy - moodY * R  // invert: high arousal = top

    // Glow
    const grd = moodCtx.createRadialGradient(dotX, dotY, 0, dotX, dotY, 14 * dpr)
    grd.addColorStop(0, 'rgba(228,122,42,0.5)')
    grd.addColorStop(1, 'rgba(228,122,42,0)')
    moodCtx.beginPath()
    moodCtx.arc(dotX, dotY, 14 * dpr, 0, Math.PI * 2)
    moodCtx.fillStyle = grd
    moodCtx.fill()

    // Dot
    moodCtx.beginPath()
    moodCtx.arc(dotX, dotY, 6 * dpr, 0, Math.PI * 2)
    moodCtx.fillStyle = '#e47a2a'
    moodCtx.fill()
    moodCtx.strokeStyle = '#fff'
    moodCtx.lineWidth = 1.5 * dpr
    moodCtx.stroke()

    // Loading indicator
    if (moodLoading) {
      moodCtx.font = `500 ${10 * dpr}px system-ui`
      moodCtx.fillStyle = 'rgba(228,122,42,0.8)'
      moodCtx.textAlign = 'center'
      moodCtx.fillText('Searching…', cx, H - 4 * dpr)
    }
  }

  function attachMoodEvents () {
    if (!moodCanvas) return

    function canvasCoordToMood (clientX, clientY) {
      const rect = moodCanvas.getBoundingClientRect()
      const dpr  = window.devicePixelRatio
      const W = moodCanvas.width
      const H = moodCanvas.height
      const cx = W / 2
      const cy = H / 2
      const R  = Math.min(cx, cy) - 8 * dpr
      // canvas CSS px → canvas logical px
      const scaleX = moodCanvas.width  / rect.width
      const scaleY = moodCanvas.height / rect.height
      const px = (clientX - rect.left) * scaleX
      const py = (clientY - rect.top)  * scaleY
      const vRaw = (px - cx) / R
      const aRaw = -(py - cy) / R  // invert Y
      return {
        valence: Math.max(-1, Math.min(1, vRaw)),
        arousal: Math.max(-1, Math.min(1, aRaw)),
      }
    }

    function onMove (clientX, clientY) {
      const { valence, arousal } = canvasCoordToMood(clientX, clientY)
      moodX = valence
      moodY = arousal
      renderMoodWheel()
      scheduleMoodSearch()
    }

    moodCanvas.addEventListener('mousedown', (e) => {
      moodDragging = true
      onMove(e.clientX, e.clientY)
    })
    document.addEventListener('mousemove', (e) => {
      if (moodDragging) onMove(e.clientX, e.clientY)
    })
    document.addEventListener('mouseup', () => { moodDragging = false })

    moodCanvas.addEventListener('touchstart', (e) => {
      e.preventDefault()
      const t = e.touches[0]
      moodDragging = true
      onMove(t.clientX, t.clientY)
    }, { passive: false })
    moodCanvas.addEventListener('touchmove', (e) => {
      e.preventDefault()
      if (!moodDragging) return
      const t = e.touches[0]
      onMove(t.clientX, t.clientY)
    }, { passive: false })
    moodCanvas.addEventListener('touchend', () => { moodDragging = false })
  }

  // ── Mood coordinate → text query ────────────────────────────────
  function moodCoordToQuery (valence, arousal) {
    if (arousal > 0.3 && valence < -0.3)  return 'tense suspenseful cinematic dark'
    if (arousal > 0.3 && valence > 0.3)   return 'energetic upbeat positive aggressive'
    if (arousal < -0.3 && valence < -0.3) return 'sad melancholy ambient calm'
    if (arousal < -0.3 && valence > 0.3)  return 'peaceful serene relaxing happy'
    return 'neutral balanced ambient'
  }

  function scheduleMoodSearch () {
    clearTimeout(moodDebounceTimer)
    moodDebounceTimer = setTimeout(runMoodSearch, 250)
  }

  async function runMoodSearch () {
    // Mood search embeds text with CLAP; without it show the inline prompt instead
    if (window.DaouchiModels && DaouchiModels.isMissing('clap')) return
    moodLoading = true
    // Compute text query from coordinates immediately (before API round-trip)
    const clientQuery = moodCoordToQuery(moodX, moodY)
    moodQuery = clientQuery

    // Update query label right away so UI reflects the zone
    const queryEl = document.getElementById('mood-query-label')
    if (queryEl) queryEl.textContent = clientQuery

    renderMoodWheel()
    updateMoodResultsUI('loading')
    try {
      const r = await fetch('/api/ai/mood_search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ valence: moodX, arousal: moodY, query: clientQuery, limit: 30 }),
      })
      if (r.status === 409) { updateMoodResultsUI('error', 'AI model not ready'); return }
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      moodResults = data.results || []
      // Prefer server-generated query label if provided, otherwise keep client one
      moodQuery = data.query || clientQuery
      updateMoodResultsUI('results')
    } catch (err) {
      updateMoodResultsUI('error', err.message)
    } finally {
      moodLoading = false
      renderMoodWheel()
    }
  }

  function updateMoodResultsUI (state, errMsg) {
    const el = document.getElementById('mood-results')
    if (!el) return

    if (state === 'loading') {
      el.innerHTML = '<div style="color:var(--muted);font-size:12px;padding:12px 0;">Searching…</div>'
      return
    }
    if (state === 'error') {
      el.innerHTML = `<div style="color:var(--red);font-size:12px;padding:8px 0;">Error: ${escHtml(errMsg || 'unknown')}</div>`
      return
    }

    // Update query label with the (possibly server-refined) query string
    const queryEl = document.getElementById('mood-query-label')
    if (queryEl && moodQuery) {
      queryEl.textContent = moodQuery.replace('this is a sound of ', '')
    }

    // Dispatch results to the main library browser
    document.dispatchEvent(new CustomEvent('dashboard-search-results', { detail: moodResults }))

    if (!moodResults.length) {
      el.innerHTML = '<div style="color:var(--muted);font-size:12px;padding:8px 0;">No results — add audio and generate embeddings first.</div>'
      return
    }

    el.innerHTML = moodResults.slice(0, 20).map(r => {
      const score = Math.round((r.mood_score || 0) * 100)
      const ext   = (r.sound && r.sound.extension || r.extension || '').toUpperCase()
      const fname = escHtml(r.sound ? r.sound.filename : r.filename || '')
      const col   = escHtml(r.sound ? r.sound.collection : r.collection || '')
      const sid   = r.sound ? r.sound.id : r.id
      return `<div class="mood-result-row" onclick="window.selectSoundById(${sid})" title="${fname}">
        <span class="badge badge-${ext.toLowerCase()}">${ext}</span>
        <span style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;">${fname}</span>
        <span style="font-size:10px;color:var(--muted);white-space:nowrap;">${col}</span>
        <span class="semantic-score-badge" style="background:${scoreColor(r.mood_score)}">${score}%</span>
      </div>`
    }).join('')
  }

  function scoreColor (score) {
    if (score >= 0.7) return '#3dba6e'
    if (score >= 0.45) return '#c9a227'
    return '#d05454'
  }

  // ═══════════════════════════════════════════════════════════════
  // Sonic Distribution Blueprint
  // ═══════════════════════════════════════════════════════════════

  async function loadSfxCategories () {
    try {
      const r = await fetch('/api/ai/sfx_categories')
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      sfxCategories = data.categories || []
      renderBlueprint()
      // Load counts lazily after categories are rendered
      loadSfxCategoryCounts()
    } catch (err) {
      console.warn('[dashboard] sfx_categories:', err)
    }
  }

  async function loadSfxCategoryCounts () {
    // Counts are CLAP-derived; skip (the Download prompt is shown) until it's ready
    if (window.DaouchiModels && DaouchiModels.isMissing('clap')) return
    try {
      const r = await fetch('/api/ai/sfx_category_counts')
      if (!r.ok) return
      const data = await r.json()
      sfxCategoryCounts = data.counts || {}
      renderBlueprint()
    } catch (err) {
      // Counts are optional — silently skip if AI not ready
    }
  }

  async function loadDistribStats () {
    distribLoading = true
    try {
      const r = await fetch('/api/sounds/stats')
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      distribStats = {
        total:  data.total  || 0,
        sfx:    data.sfx_count   || 0,
        music:  data.music_count || 0,
        stems:  data.with_stems  || 0,
        withEmbeddings: data.with_embeddings || 0,
        totalDuration:  data.total_duration  || 0,
        tagCount:       data.tag_count       || 0,
        collections:    data.collections     || [],
        topTags:        data.top_tags        || [],
      }
      renderBlueprint()
      renderTagCloud()
      renderCollectionBreakdown()
    } catch (err) {
      console.warn('[dashboard] stats:', err)
    } finally {
      distribLoading = false
    }
  }

  function renderBlueprint () {
    const el = document.getElementById('sonic-blueprint')
    if (!el) return
    const { total, sfx, music } = distribStats
    if (!total) {
      el.innerHTML = '<div style="color:var(--muted);font-size:12px;">No sounds indexed yet.</div>'
      return
    }

    const musicPct = total ? (music / total * 100) : 0
    const sfxPct   = total ? (sfx   / total * 100) : 0

    el.innerHTML = `
      <div class="blueprint-bars">
        <div class="blueprint-bar-row">
          <span class="blueprint-bar-label">♫ Music</span>
          <div class="blueprint-bar-track">
            <div class="blueprint-bar-fill" style="width:${musicPct.toFixed(1)}%;background:var(--blue)"></div>
          </div>
          <span class="blueprint-bar-count">${music}</span>
        </div>
        <div class="blueprint-bar-row">
          <span class="blueprint-bar-label">🔊 SFX</span>
          <div class="blueprint-bar-track">
            <div class="blueprint-bar-fill" style="width:${sfxPct.toFixed(1)}%;background:var(--accent)"></div>
          </div>
          <span class="blueprint-bar-count">${sfx}</span>
        </div>
      </div>
      <div class="sfx-categories">
        <div class="sfx-category-title">SFX by Role <span style="font-size:10px;color:var(--muted);">(click to filter)</span></div>
        ${(() => {
          const maxCount = Math.max(1, ...sfxCategories.map(c => sfxCategoryCounts[c.id] || 0))
          return sfxCategories.map(cat => {
            const count = sfxCategoryCounts[cat.id] || 0
            const pct = count > 0 ? (count / maxCount * 100).toFixed(1) : 0
            return `
            <button class="sfx-cat-btn" onclick="window.searchByCategory('${escAttr(cat.id)}')" data-cat="${escAttr(cat.id)}">
              <span class="sfx-cat-emoji">${cat.emoji}</span>
              <span class="sfx-cat-label">${escHtml(cat.label)}</span>
              <div class="sfx-cat-bar-track">
                <div class="sfx-cat-bar-fill" style="width:${pct}%"></div>
              </div>
              <span class="sfx-cat-count">${count > 0 ? count : '—'}</span>
            </button>`
          }).join('')
        })()}
        ${!sfxCategories.length ? '<div style="color:var(--muted);font-size:11px;">Enable AI to classify SFX by role.</div>' : ''}
      </div>
    `
  }

  function renderTagCloud () {
    const el = document.getElementById('tag-cloud')
    if (!el || !distribStats.topTags) return
    const tags = distribStats.topTags
    if (!tags.length) {
      el.innerHTML = '<span style="color:var(--muted);font-size:12px;">No tags yet.</span>'
      return
    }
    const maxCount = tags[0].count || 1
    el.innerHTML = tags.slice(0, 40).map(t => {
      const size = 10 + Math.round((t.count / maxCount) * 10)
      const opacity = 0.5 + (t.count / maxCount) * 0.5
      return `<button class="tag-cloud-item" style="font-size:${size}px;opacity:${opacity}" onclick="window.filterByTag('${escAttr(t.tag)}')">#${escHtml(t.tag)} <sup style="font-size:9px;color:var(--muted);">${t.count}</sup></button>`
    }).join('')
  }

  function renderCollectionBreakdown () {
    const el = document.getElementById('collection-breakdown-list')
    if (!el || !distribStats.collections) return
    const cols = distribStats.collections
    if (!cols.length) {
      el.innerHTML = '<div style="color:var(--muted);font-size:12px;">No collections.</div>'
      return
    }
    const maxCount = cols[0].count || 1
    el.innerHTML = cols.map(c => {
      const pct = (c.count / maxCount * 100).toFixed(1)
      const color = collectionColor(c.name)
      const dur = formatDuration(c.avg_duration)
      return `<div class="blueprint-col-row" onclick="window.filterByCollection('${escAttr(c.name)}')">
        <div class="blueprint-col-bar" style="width:${pct}%;background:${color}"></div>
        <span class="blueprint-col-name">${escHtml(c.name)}</span>
        <span style="font-size:10px;color:var(--muted);white-space:nowrap;">${c.count} · ${dur}</span>
      </div>`
    }).join('')
  }

  // ═══════════════════════════════════════════════════════════════
  // Global helpers (called from inline HTML and app.js)
  // ═══════════════════════════════════════════════════════════════

  window.searchByCategory = async function (catId) {
    const el = document.getElementById('sfx-category-results')
    if (!el) return
    if (window.DaouchiModels && DaouchiModels.isMissing('clap')) {
      el.innerHTML = '<div data-model-gate="clap"></div>'
      DaouchiModels.renderGates(el)
      return
    }
    el.innerHTML = '<div style="color:var(--muted);font-size:12px;padding:8px 0;">Searching…</div>'
    try {
      const r = await fetch('/api/ai/sfx_search', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ category: catId, limit: 30 }),
      })
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      const results = data.results || []

      // Also send results to main library browser via a custom event
      document.dispatchEvent(new CustomEvent('dashboard-search-results', { detail: results }))

      el.innerHTML = results.slice(0, 10).map(s => {
        const ext = (s.extension || '').toUpperCase()
        return `<div class="mood-result-row" onclick="window.selectSoundById(${s.id})" title="${escHtml(s.filename)}">
          <span class="badge badge-${ext.toLowerCase()}">${ext}</span>
          <span style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;font-size:12px;">${escHtml(s.filename)}</span>
          <span style="font-size:10px;color:var(--muted);">${escHtml(s.collection || '')}</span>
        </div>`
      }).join('') || '<div style="color:var(--muted);font-size:12px;">No results — generate embeddings first.</div>'
    } catch (err) {
      el.innerHTML = `<div style="color:var(--red);font-size:12px;">Error: ${escHtml(err.message)}</div>`
    }
  }

  window.filterByTag = function (tag) {
    document.dispatchEvent(new CustomEvent('dashboard-filter-tag', { detail: tag }))
  }

  window.filterByCollection = function (name) {
    document.dispatchEvent(new CustomEvent('dashboard-filter-collection', { detail: name }))
  }

  window.selectSoundById = function (id) {
    document.dispatchEvent(new CustomEvent('select-sound-by-id', { detail: { id } }))
  }

  // ═══════════════════════════════════════════════════════════════
  // Utilities
  // ═══════════════════════════════════════════════════════════════
  function collectionColor (name) {
    let hash = 0
    for (let i = 0; i < name.length; i++) hash = name.charCodeAt(i) + ((hash << 5) - hash)
    const hue = Math.abs(hash) % 360
    return `hsl(${hue}, 55%, 55%)`
  }

  function formatDuration (secs) {
    if (!secs) return '0:00'
    const s = Math.floor(secs)
    const m = Math.floor(s / 60)
    const h = Math.floor(m / 60)
    if (h > 0) return `${h}:${String(m % 60).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`
    return `${m}:${String(s % 60).padStart(2, '0')}`
  }

  function escHtml (str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;')
  }

  function escAttr (str) {
    return String(str).replace(/\\/g, '\\\\').replace(/'/g, "\\'")
  }

})()
