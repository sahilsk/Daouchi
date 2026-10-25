;(function () {
  'use strict'

  let ws          = null   // WaveSurfer instance
  let wsReady     = false  // true once 'ready' fires for the current ws instance
  let currentSound = null
  let inPoint     = 0.0    // fraction 0..1
  let outPoint    = 1.0    // fraction 0..1
  let dragging    = null   // 'in' | 'out' | null
  let loopRegion  = false
  let _seeking    = false
  let isPlaying   = false
  let autoplay    = localStorage.getItem('daouchi_autoplay') === 'true'

  // J-K-L NLE playback engine state
  // jklSpeed: positive = forward, negative = reverse; magnitude is multiplier (1/2/4)
  let jklSpeed    = 0   // 0 = stopped

  // Flag set by row play button click: play the next loaded sound even if autoplay is off
  let _playAfterLoad = false
  document.addEventListener('play-after-load', () => { _playAfterLoad = true })

  // Waveform zoom state
  let zoomLevel = 1  // px-per-second multiplier

  // Hover preview state
  let previewWs      = null
  let previewSoundId = null

  document.addEventListener('autoplay-changed', (e) => { autoplay = e.detail })

  document.addEventListener('export-clip', () => {
    if (ws && wsReady && currentSound) window.exportClip()
  })

  /* ── In/Out point custom event listeners ────────────────────────── */
  document.addEventListener('set-inpoint', (e) => {
    if (!ws || !wsReady) return
    const timeSec = (e && e.detail != null) ? e.detail : ws.getCurrentTime()
    const dur = ws.getDuration()
    if (!dur) return
    inPoint = Math.max(0, Math.min(timeSec / dur, outPoint - 0.01))
    updateMarkers()
    updateInOutReadout()
  })

  document.addEventListener('set-outpoint', (e) => {
    if (!ws || !wsReady) return
    const timeSec = (e && e.detail != null) ? e.detail : ws.getCurrentTime()
    const dur = ws.getDuration()
    if (!dur) return
    outPoint = Math.max(inPoint + 0.01, Math.min(timeSec / dur, 1))
    updateMarkers()
    updateInOutReadout()
  })

  document.addEventListener('clear-inout', () => {
    inPoint  = 0.0
    outPoint = 1.0
    updateMarkers()
    updateInOutReadout()
  })

  document.addEventListener('seek-to-inpoint', () => {
    if (ws && wsReady) ws.seekTo(inPoint)
  })

  document.addEventListener('seek-to-outpoint', () => {
    if (ws && wsReady) ws.seekTo(outPoint)
  })

  /* ── DOM refs (resolved after DOMContentLoaded) ─────────────── */
  let playerPanel, waveformEl, waveformContainer
  let inMarker, outMarker, regionOverlay
  let playerFilename, playerCollection, playerTime
  let playerTagsEl, playerSuggestedTagsEl
  let stemsContainer, copyPathBtn, statusToast, splitStemsBtn
  let inOutReadoutEl

  let playerArtworkEl = null

  function resolveRefs() {
    playerPanel          = document.getElementById('player-panel')
    waveformEl           = document.getElementById('waveform')
    waveformContainer    = document.getElementById('waveform-container')
    inMarker             = document.getElementById('in-marker')
    outMarker            = document.getElementById('out-marker')
    regionOverlay        = document.getElementById('region-overlay')
    playerFilename       = document.getElementById('player-filename')
    playerCollection     = document.getElementById('player-collection')
    playerTime           = document.getElementById('player-time')
    playerTagsEl         = document.getElementById('player-tags')
    playerSuggestedTagsEl= document.getElementById('player-suggested-tags')
    stemsContainer       = document.getElementById('stems-container')
    copyPathBtn          = document.getElementById('copy-path-btn')
    statusToast         = document.getElementById('status-toast')
    splitStemsBtn        = document.getElementById('split-stems-btn')
    inOutReadoutEl       = document.getElementById('inout-readout')
    playerArtworkEl      = document.getElementById('player-artwork')
  }

  function loadArtwork(sound) {
    if (!playerArtworkEl) playerArtworkEl = document.getElementById('player-artwork')
    if (!playerArtworkEl) return
    playerArtworkEl.style.display = 'none'
    playerArtworkEl.src = ''
    playerArtworkEl.onload  = () => { playerArtworkEl.style.display = 'block' }
    playerArtworkEl.onerror = () => { playerArtworkEl.style.display = 'none' }
    playerArtworkEl.src = `/api/sounds/${sound.id}/artwork`
  }

  /* ── Label/rating strip helpers ─────────────────────────────── */
  function updateLabelRatingStrip(sound) {
    const strip = document.getElementById('player-label-rating')
    if (!strip) return
    strip.style.display = 'flex'
    // Update dot button active states
    for (let i = 0; i <= 5; i++) {
      const btn = document.getElementById('plr-l' + i)
      if (btn) btn.classList.toggle('active', (sound.label || 0) === i)
    }
  }

  window.playerSetLabel = async (labelVal) => {
    if (!currentSound) return
    const prev = currentSound.label || 0
    currentSound.label = labelVal
    updateLabelRatingStrip(currentSound)
    // Sync to app.js Alpine state via event so the row label dot updates too
    document.dispatchEvent(new CustomEvent('sound-label-changed', { detail: { id: currentSound.id, label: labelVal } }))
    try {
      const r = await fetch(`/api/sounds/${currentSound.id}/label`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ label: labelVal }),
      })
      if (!r.ok) throw new Error(r.statusText)
    } catch (_) {
      currentSound.label = prev
      updateLabelRatingStrip(currentSound)
      document.dispatchEvent(new CustomEvent('sound-label-changed', { detail: { id: currentSound.id, label: prev } }))
    }
  }

  /* ── In/Out timecode readout ─────────────────────────────────────── */
  function formatTimecode(frac) {
    if (!ws || !wsReady) return '—'
    const totalSec = frac * ws.getDuration()
    const m   = Math.floor(totalSec / 60)
    const s   = Math.floor(totalSec % 60)
    const ms  = Math.round((totalSec % 1) * 1000)
    return m + ':' + String(s).padStart(2, '0') + '.' + String(ms).padStart(3, '0')
  }

  function updateInOutReadout() {
    if (!inOutReadoutEl) inOutReadoutEl = document.getElementById('inout-readout')
    if (!inOutReadoutEl) return
    const inEl  = inOutReadoutEl.querySelector('.inout-in-time')
    const outEl = inOutReadoutEl.querySelector('.inout-out-time')
    if (inEl)  inEl.textContent  = formatTimecode(inPoint)
    if (outEl) outEl.textContent = formatTimecode(outPoint)
  }

  /* ── Collapsible sections ────────────────────────────────────── */
  function initCollapsibles() {
    document.querySelectorAll('.player-collapsible-header').forEach(header => {
      const body = header.nextElementSibling
      if (!body) return
      header.addEventListener('click', (e) => {
        // Buttons inside the header (refresh suggestions, split stems) must not toggle the section
        if (e.target.closest('button')) return
        const isOpen = header.classList.contains('open')
        header.classList.toggle('open', !isOpen)
        if (isOpen) {
          // currently open → closing
          // Set explicit px height first (can't transition from 'none'), force reflow, then animate to 0
          body.style.maxHeight = body.scrollHeight + 'px'
          body.offsetHeight  // force reflow so browser registers the starting height
          requestAnimationFrame(() => {
            body.style.maxHeight = '0px'
            body.classList.add('collapsed')
          })
        } else {
          body.classList.remove('collapsed')
          body.style.maxHeight = body.scrollHeight + 'px'
          setTimeout(() => { body.style.maxHeight = 'none' }, 210)
        }
      })
      // Initialize state
      const startOpen = header.dataset.startOpen === 'true'
      if (startOpen) {
        header.classList.add('open')
        body.style.maxHeight = 'none'
      } else {
        body.classList.add('collapsed')
        body.style.maxHeight = '0px'
      }
    })
  }

  /* ── DOMContentLoaded init ───────────────────────────────────── */
  document.addEventListener('DOMContentLoaded', () => {
    resolveRefs()
    // Player panel is always visible
    if (playerPanel) playerPanel.style.display = 'flex'
    initCollapsibles()
  })

  // Module-level stems poll handle — cleared on each new sound selection
  let stemPollInterval = null
  let stemPollTimeout  = null

  /* ── AI tag confidence color coding ─────────────────────────── */
  function scoreColor(score) {
    if (score >= 0.8) return '#3dba6e'
    if (score >= 0.5) return '#c9a227'
    return '#d05454'
  }
  window.scoreColor = scoreColor

  /* ── Sound selection ─────────────────────────────────────────── */
  document.addEventListener('sound-selected', async (e) => {
    const sound   = e.detail
    currentSound  = sound
    inPoint       = 0.0
    outPoint      = 1.0
    isPlaying     = false
    wsReady       = false
    zoomLevel     = 1
    _zoomListenerAttached = false

    // Clear any outstanding stems poll from a previous sound
    if (stemPollInterval) { clearInterval(stemPollInterval); stemPollInterval = null }
    if (stemPollTimeout)  { clearTimeout(stemPollTimeout);   stemPollTimeout  = null }

    resolveRefs()

    // Show player content, hide empty state
    const playerContent  = document.getElementById('player-content')
    const playerEmpty    = document.getElementById('player-empty-state')
    if (playerContent) playerContent.style.display = 'flex'
    if (playerEmpty)   playerEmpty.style.display   = 'none'

    // Update header fields
    if (playerFilename)   playerFilename.textContent   = sound.filename
    if (playerCollection) playerCollection.textContent = sound.collection || sound.folder_path || ''
    if (playerTime)       playerTime.textContent       = '0:00 / —'
    if (statusToast)     statusToast.textContent     = ''
    clearTimeout(_toastTimer)
    if (_suggestedTagsPollTimer) { clearTimeout(_suggestedTagsPollTimer); _suggestedTagsPollTimer = null }
    // Export result is per sound: start clean, restore this sound's last export if any
    renderExportResult(sound.id)

    // Load artwork thumbnail
    loadArtwork(sound)

    // Reset in/out readout for the new sound
    if (!inOutReadoutEl) inOutReadoutEl = document.getElementById('inout-readout')
    if (inOutReadoutEl) {
      const inEl  = inOutReadoutEl.querySelector('.inout-in-time')
      const outEl = inOutReadoutEl.querySelector('.inout-out-time')
      if (inEl)  inEl.textContent  = '0:00.000'
      if (outEl) outEl.textContent = '—'
    }

    // Reset copy-path button if it's stuck in "Copied!" state
    const copyBtn = document.getElementById('copy-path-btn')
    if (copyBtn) {
      copyBtn.classList.remove('copied')
      copyBtn.removeAttribute('data-copied')
      clearTimeout(copyBtn._copyTimer)
    }

    // Populate metadata grid
    const metaGrid = document.getElementById('player-meta-grid')
    if (metaGrid) {
      const fmt = (label, val, accent) => `
        <div>
          <div style="font-size:9px;color:var(--muted);text-transform:uppercase;letter-spacing:.05em;margin-bottom:1px;">${label}</div>
          <div style="color:${accent || 'var(--text)'};font-variant-numeric:tabular-nums;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${val}</div>
        </div>`
      const dur = sound.duration != null
        ? (sound.duration < 60
            ? sound.duration.toFixed(1) + 's'
            : Math.floor(sound.duration/60) + 'm ' + Math.floor(sound.duration%60) + 's')
        : '—'
      const size = sound.size != null
        ? (sound.size > 1048576 ? (sound.size/1048576).toFixed(1) + ' MB' : Math.round(sound.size/1024) + ' KB')
        : '—'
      const sr = sound.sample_rate ? (sound.sample_rate/1000).toFixed(1) + ' kHz' : '—'
      const bd = sound.bit_depth   ? sound.bit_depth + '-bit' : '—'
      const br = sound.bitrate     ? Math.round(sound.bitrate/1000) + ' kbps' : '—'
      const ext = (sound.extension || '?').toUpperCase()
      const src = sound.source ? sound.source.charAt(0).toUpperCase() + sound.source.slice(1) : '—'
      const col = sound.collection || '—'
      const folder = sound.folder_path || '—'
      const indexed = sound.indexed_at
        ? new Date(sound.indexed_at).toLocaleDateString(undefined, { year:'numeric', month:'short', day:'numeric' })
        : '—'
      const modified = sound.file_modified_at
        ? new Date(sound.file_modified_at).toLocaleDateString(undefined, { year:'numeric', month:'short', day:'numeric' })
        : '—'
      const rating = sound.rating || 0
      const ratingStars = rating > 0
        ? '★'.repeat(rating) + '☆'.repeat(5 - rating)
        : '—'
      const LABEL_COLORS = ['', '#ff3b30', '#ff9f0a', '#ffd60a', '#30d158', '#0a84ff']
      const LABEL_NAMES  = ['No label', 'Red', 'Orange', 'Yellow', 'Green', 'Blue']
      const labelIdx = sound.label || 0
      const labelHtml = labelIdx > 0
        ? `<span style="display:inline-flex;align-items:center;gap:4px;">
             <svg width="11" height="11" viewBox="0 0 24 24" fill="${LABEL_COLORS[labelIdx]}"><path d="M20.59 13.41l-7.17 7.17a2 2 0 0 1-2.83 0L2 12V2h10l8.59 8.59a2 2 0 0 1 0 2.82z"/></svg>
             <span style="color:${LABEL_COLORS[labelIdx]}">${LABEL_NAMES[labelIdx]}</span>
           </span>`
        : '—'
      metaGrid.innerHTML =
        fmt('Duration', dur, 'var(--green)') +
        fmt('Size', size) +
        fmt('Format', ext, 'var(--accent)') +
        fmt('Sample Rate', sr) +
        fmt('Bit Depth', bd) +
        fmt('Bitrate', br) +
        fmt('Source', src) +
        fmt('Collection', col) +
        fmt('Label', labelHtml) +
        fmt('Rating', `<span style="color:#f5a623;letter-spacing:1px;">${ratingStars}</span>`) +
        fmt('File Modified', modified, 'var(--text-dim)') +
        `<div style="grid-column:1/-1">
          <div style="font-size:9px;color:var(--muted);text-transform:uppercase;letter-spacing:.05em;margin-bottom:1px;">Folder</div>
          <div style="color:var(--text-dim);font-size:10px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;" title="${folder}">${folder}</div>
        </div>` +
        `<div style="grid-column:1/-1">
          <div style="font-size:9px;color:var(--muted);text-transform:uppercase;letter-spacing:.05em;margin-bottom:1px;">Indexed (added to library)</div>
          <div style="color:var(--text-dim);font-size:10px;">${indexed}</div>
        </div>`
    }

    // Update split-stems button state
    if (splitStemsBtn) {
      if (sound.has_stems) {
        splitStemsBtn.textContent = 'Stems Ready'
        splitStemsBtn.disabled    = false
      } else {
        splitStemsBtn.textContent = 'Split Stems'
        splitStemsBtn.disabled    = false
      }
    }

    // Update play button icon
    setPlayIcon(false)

    // Update label/rating strip
    updateLabelRatingStrip(sound)

    // Load tags
    loadPlayerTags(sound)

    // Check stems status
    loadStemsStatus(sound)

    // Destroy previous WaveSurfer and stem wavesurfers
    Object.values(stemWaveSurfers).forEach(sw => {
      try { sw.pause() } catch (_) {}
      try { sw.destroy() } catch (_) {}
    })
    stemWaveSurfers = {}
    if (ws) {
      try { ws.pause() } catch (_) {}
      try { ws.destroy() } catch (_) {}
      ws = null
    }
    isPlaying = false

    waveformEl.innerHTML = ''
    // Eagerly clear stale transient ticks regardless of whether the new
    // sound's transient fetch succeeds (fixes ticks persisting on 404/network error).
    if (waveformContainer) {
      waveformContainer.querySelectorAll('.transient-tick').forEach(el => el.remove())
    }

    // If a preview ws was pre-loaded for this sound, destroy it now
    // (we'll create a fresh authoritative instance below)
    if (previewWs) {
      try { previewWs.destroy() } catch (_) {}
      if (previewWs && previewWs._previewDiv) {
        try { previewWs._previewDiv.remove() } catch (_) {}
      }
      previewWs = null
    }
    previewSoundId = null

    try {
      // Capture the new instance in a local const so that all callbacks
      // below close over this specific instance, not the module-level `ws`
      // which may be reassigned before any of them fire.
      const localWs = WaveSurfer.create({
        container:     '#waveform',
        waveColor:     '#3d3d3d',
        progressColor: '#e47a2a',
        cursorColor:   '#e47a2a',
        barWidth:      2,
        barGap:        1,
        height:        100,
        normalize:     true,
      })
      ws = localWs

      localWs.load(`/audio/stream?path=${encodeURIComponent(sound.path)}`)

      localWs.on('ready', () => {
        // Discard events from superseded instances
        if (localWs !== ws) return
        wsReady = true
        const exportBtn = document.getElementById('export-btn')
        if (exportBtn) exportBtn.disabled = false
        updateMarkers()
        updateInOutReadout()
        loadTransients(sound, localWs)
        // Wire audio FX chain through the media element
        setupAudioChain(localWs.getMediaElement ? localWs.getMediaElement() : null)
        // Apply stored volume
        try { localWs.setVolume(_volume) } catch (_) {}
        // Attach zoom wheel listener now that waveformContainer is confirmed live
        attachZoomListener()
        const shouldPlay = autoplay || _playAfterLoad
        _playAfterLoad = false
        if (shouldPlay) {
          // Tiny delay ensures the audio context is fully resumed after user
          // interaction (required by browser autoplay policy).
          setTimeout(() => {
            if (localWs !== ws) return
            localWs.play().catch(() => {})
          }, 80)
        }
      })

      localWs.on('play',  () => {
        if (localWs !== ws) return
        isPlaying = true
        setPlayIcon(true)
        document.dispatchEvent(new CustomEvent('player-playstate', { detail: { soundId: sound.id, playing: true } }))
      })
      localWs.on('pause', () => {
        if (localWs !== ws) return
        isPlaying = false
        setPlayIcon(false)
        document.dispatchEvent(new CustomEvent('player-playstate', { detail: { soundId: sound.id, playing: false } }))
      })
      localWs.on('finish',() => {
        if (localWs !== ws) return
        isPlaying = false
        setPlayIcon(false)
        document.dispatchEvent(new CustomEvent('player-playstate', { detail: { soundId: sound.id, playing: false } }))
      })

      localWs.on('timeupdate', (time) => {
        // Discard stale timeupdate events from a previous instance
        if (localWs !== ws) return
        const dur = localWs.getDuration()
        playerTime.textContent = formatTime(time) + ' / ' + formatTime(dur)

        // Loop region enforcement
        if (loopRegion && dur > 0 && time >= outPoint * dur && !_seeking) {
          _seeking = true
          localWs.seekTo(inPoint)
          setTimeout(() => { _seeking = false }, 50)
        }
      })

      localWs.on('error', (err) => {
        if (localWs !== ws) return
        console.error('WaveSurfer error:', err)
        if (waveformEl) waveformEl.innerHTML =
          `<p style="color:#d09090;font-size:11px;padding:8px 0;">Audio load error: ${err}</p>`
      })
    } catch (err) {
      console.error('WaveSurfer create error:', err)
    }

    // Player panel is always flex — no display toggle needed here
    // (already set on DOMContentLoaded and never hidden)

    // Disable export button until waveform is ready
    const exportBtn = document.getElementById('export-btn')
    if (exportBtn) exportBtn.disabled = true
  })

  /* ── Waveform zoom with Ctrl+scroll ─────────────────────────── */
  let _zoomListenerAttached = false
  function attachZoomListener() {
    if (_zoomListenerAttached || !waveformContainer) return
    _zoomListenerAttached = true
    waveformContainer.addEventListener('wheel', (e) => {
      if (!ws || !e.ctrlKey) return
      e.preventDefault()
      const delta = e.deltaY > 0 ? -1 : 1
      zoomLevel = Math.max(1, Math.min(20, zoomLevel + delta))
      ws.zoom(zoomLevel * 50)  // WaveSurfer minPxPerSec
      // Update zoom indicator
      const zoomEl = document.getElementById('zoom-indicator')
      if (zoomEl) zoomEl.textContent = zoomLevel > 1 ? zoomLevel + '×' : ''
    }, { passive: false })
  }

  /* ── Hover preview in results ────────────────────────────────── */
  document.addEventListener('result-row-hovered', (e) => {
    const sound = e.detail
    if (previewSoundId === sound.id) return
    // Cancel previous preview
    if (previewWs) {
      try { previewWs.destroy() } catch (_) {}
      if (previewWs._previewDiv) { try { previewWs._previewDiv.remove() } catch (_) {} }
      previewWs = null
    }
    previewSoundId = sound.id
    // After 500ms dwell, start loading
    clearTimeout(window._previewTimer)
    window._previewTimer = setTimeout(() => {
      if (previewSoundId !== sound.id) return
      const div = document.createElement('div')
      div.style.cssText = 'position:absolute;left:-9999px;top:-9999px;width:300px;height:80px;'
      document.body.appendChild(div)
      try {
        previewWs = WaveSurfer.create({ container: div, height: 80 })
        previewWs.load('/audio/stream?path=' + encodeURIComponent(sound.path))
        previewWs._previewDiv = div
      } catch (_) {}
    }, 500)
  })

  /* ── radar-find-similar listener ─────────────────────────────── */
  document.addEventListener('radar-find-similar', (e) => {
    const { sound_id } = e.detail
    // Dispatch to app.js to do semantic search by sound_id
    document.dispatchEvent(new CustomEvent('find-similar-by-id', { detail: { sound_id } }))
  })

  /* ── Play icon helper ────────────────────────────────────────── */
  function setPlayIcon(playing) {
    const btn = document.getElementById('play-btn')
    if (btn) btn.textContent = playing ? '⏸' : '▶'
  }

  /* ── Marker drag logic ───────────────────────────────────────── */
  document.addEventListener('mousedown', (e) => {
    if (!waveformContainer) return
    const rect = waveformContainer.getBoundingClientRect()
    if (e.clientX < rect.left || e.clientX > rect.right) return
    if (e.clientY < rect.top  || e.clientY > rect.bottom) return

    const x   = e.clientX - rect.left
    const inX = inPoint  * rect.width
    const outX= outPoint * rect.width

    if (Math.abs(x - inX)  < 10) { dragging = 'in';  e.preventDefault() }
    else if (Math.abs(x - outX) < 10) { dragging = 'out'; e.preventDefault() }
  })

  document.addEventListener('mousemove', (e) => {
    if (!dragging || !waveformContainer) return
    const rect = waveformContainer.getBoundingClientRect()
    const frac = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width))
    if (dragging === 'in')  inPoint  = Math.min(frac, outPoint - 0.01)
    if (dragging === 'out') outPoint = Math.max(frac, inPoint  + 0.01)
    updateMarkers()
  })

  document.addEventListener('mouseup', () => { dragging = null })

  /* ── Marker / region positioning ────────────────────────────── */
  function updateMarkers() {
    if (!waveformContainer) return
    const w = waveformContainer.offsetWidth
    if (inMarker)     inMarker.style.left     = (inPoint  * w) + 'px'
    if (outMarker)    outMarker.style.left    = (outPoint * w) + 'px'
    if (regionOverlay) {
      regionOverlay.style.left  = (inPoint  * w) + 'px'
      regionOverlay.style.width = ((outPoint - inPoint) * w) + 'px'
    }
    updateInOutReadout()
  }

  // Keep markers in sync when window resizes; also reflow WaveSurfer canvas
  window.addEventListener('resize', () => {
    updateMarkers()
    if (ws && wsReady) {
      const newWidth = waveformEl ? waveformEl.offsetWidth : 0
      if (newWidth > 0) ws.setOptions({ width: newWidth })
    }
  })

  /* ── Transient heatmap ticks ─────────────────────────────────── */
  async function loadTransients(sound, currentWs) {
    try {
      const r = await fetch(`/api/sounds/${sound.id}/transients`)
      if (!r.ok) return
      const data = await r.json()
      // Guard: discard if a newer sound has been selected since fetch started
      if (currentWs !== ws) return
      if (!waveformContainer || !ws) return

      // Remove stale ticks
      waveformContainer.querySelectorAll('.transient-tick').forEach(el => el.remove())

      const w = waveformContainer.offsetWidth
      // fractions: 0..1 positions returned by /api/sounds/:id/transients
      ;(data.fractions || []).forEach(frac => {
        const tick       = document.createElement('div')
        tick.className   = 'transient-tick'
        tick.style.left  = (frac * w) + 'px'
        waveformContainer.appendChild(tick)
      })
    } catch (_) { /* transients endpoint offline — silently skip */ }
  }

  /* ── Tags ────────────────────────────────────────────────────── */
  function showSuggestedTagsLoading() {
    if (!playerSuggestedTagsEl) return
    playerSuggestedTagsEl.innerHTML =
      '<span style="display:inline-flex;align-items:center;gap:6px;font-size:11px;color:var(--muted);">' +
      '<span class="spinner" style="width:9px;height:9px;border-width:1.5px;margin:0;"></span>' +
      'Analysing…</span>'
  }

  // Suggestions need AST + CLAP; the inline Download prompt lives above the list
  function suggestionsBlocked() {
    return !!(window.DaouchiModels && DaouchiModels.missing(['ast', 'clap']).length)
  }

  const MODELS_NEEDED_HTML =
    '<span style="font-size:11px;color:var(--muted);">Suggestions are available once the AI models are downloaded</span>'

  // Models finished downloading while a sound is open → fetch its suggestions now
  let _wasBlocked = false
  document.addEventListener('models-changed', () => {
    const blocked = suggestionsBlocked()
    if (_wasBlocked && !blocked && currentSound) loadPlayerTags(currentSound)
    _wasBlocked = blocked
  })

  async function loadPlayerTags(sound) {
    renderConfirmedTags(sound.tags || [], sound.id)
    if (suggestionsBlocked()) {
      if (playerSuggestedTagsEl) playerSuggestedTagsEl.innerHTML = MODELS_NEEDED_HTML
      return
    }
    showSuggestedTagsLoading()

    const soundAtFetch = sound
    try {
      const r    = await fetch(`/api/sounds/${sound.id}/suggested_tags`)
      if (!currentSound || currentSound.id !== soundAtFetch.id) return
      if (r.status === 409) {
        if (playerSuggestedTagsEl) playerSuggestedTagsEl.innerHTML = MODELS_NEEDED_HTML
        return
      }
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      if (data.suggestions && data.suggestions.length > 0) {
        renderSuggestedTags(data.suggestions, sound.id)
      } else {
        // Tags not ready yet — autotag was just kicked off, poll for them
        pollSuggestedTags(sound.id, sound)
      }
    } catch (_) {
      renderSuggestedTags([], sound.id)
    }
  }

  let _suggestedTagsPollTimer = null

  function pollSuggestedTags(soundId, sound) {
    if (_suggestedTagsPollTimer) clearTimeout(_suggestedTagsPollTimer)
    let attempts = 0
    let consecutiveErrors = 0
    const poll = async () => {
      // Stop if the user has switched to a different sound
      if (!currentSound || currentSound.id !== soundId) return
      attempts++
      try {
        const r = await fetch(`/api/sounds/${soundId}/suggested_tags`)
        if (r.status === 409) {   // model_not_ready — the Download prompt takes over
          if (playerSuggestedTagsEl) playerSuggestedTagsEl.innerHTML = MODELS_NEEDED_HTML
          return
        }
        // Permanent or definitive server errors — stop retrying
        if (!r.ok) {
          if (r.status === 404 || r.status === 403 || r.status === 401) {
            renderSuggestedTags([], soundId)
            return
          }
          // For transient errors (5xx, 429) count consecutive failures
          consecutiveErrors++
          if (consecutiveErrors >= 3) {
            renderSuggestedTags([], soundId)
            return
          }
          // Fall through to reschedule
        } else {
          consecutiveErrors = 0
          const data = await r.json()
          if (data.suggestions && data.suggestions.length > 0) {
            renderSuggestedTags(data.suggestions, soundId)
            return
          }
        }
      } catch (_) {}
      if (attempts < 30) {
        _suggestedTagsPollTimer = setTimeout(poll, 2000)
      } else {
        if (playerSuggestedTagsEl)
          playerSuggestedTagsEl.innerHTML =
            '<span style="font-size:11px;color:var(--muted);">No suggestions found</span>'
      }
    }
    _suggestedTagsPollTimer = setTimeout(poll, 1500)
  }

  function renderConfirmedTags(tags, soundId) {
    if (!playerTagsEl) return
    playerTagsEl.innerHTML = tags.map(t =>
      `<span class="tag-chip">${escHtml(t)} <button onclick="removeTag(${soundId},${escAttr(t)})">×</button></span>`
    ).join('')
  }

  function renderSuggestedTags(suggestions, soundId) {
    if (!playerSuggestedTagsEl) return
    if (!suggestions.length) {
      playerSuggestedTagsEl.innerHTML =
        '<span style="font-size:11px;color:var(--muted);">No suggestions — try ↺ Refresh</span>'
      return
    }
    playerSuggestedTagsEl.innerHTML = suggestions.map(s =>
      `<span class="tag-suggested" onclick="confirmTag(${soundId},${escAttr(s.tag)})" title="Click to confirm">` +
      `<span class="tag-confirm-icon">+</span>${escHtml(s.tag)} ` +
      `<span style="color:${scoreColor(s.score)};opacity:0.9;font-size:10px;">${Math.round(s.score * 100)}%</span>` +
      `</span>`
    ).join('')
  }

  /* ── Add tag from player input ───────────────────────────────── */
  window.addTagFromInput = async () => {
    const input = document.getElementById('player-tag-input')
    if (!input || !currentSound) return
    const tag = input.value.trim().toLowerCase()
    if (!tag) return
    input.value = ''

    // Optimistically update local state and render
    const soundAtCallTime = currentSound
    if (!soundAtCallTime.tags.includes(tag)) soundAtCallTime.tags.push(tag)
    renderConfirmedTags(soundAtCallTime.tags, soundAtCallTime.id)

    try {
      const r = await fetch(`/api/sounds/${soundAtCallTime.id}/tags`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ add: [tag] }),
      })
      if (!r.ok) throw new Error(r.statusText)
    } catch (err) {
      // Roll back optimistic update
      soundAtCallTime.tags = soundAtCallTime.tags.filter(t => t !== tag)
      if (currentSound && currentSound.id === soundAtCallTime.id) {
        renderConfirmedTags(soundAtCallTime.tags, soundAtCallTime.id)
        if (statusToast) {
          statusToast.textContent = '✗ Failed to save tag'
          statusToast.style.color = '#d09090'
          setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 3000)
        }
      }
    }
  }

  let stemWaveSurfers = {}  // ch → WaveSurfer instance

  /* ── Stems ───────────────────────────────────────────────────── */
  async function loadStemsStatus(sound) {
    try {
      const r    = await fetch(`/api/stems/${sound.id}`)
      if (!r.ok) {
        const body = await r.json().catch(() => null)
        if (body && body.status === 'not_available') { showStemsUnavailable(); return }
        if (stemsContainer) stemsContainer.innerHTML = '<p style="color:var(--muted);font-size:11px;">No stems yet.</p>'
        return
      }
      const data = await r.json()
      if (data.status === 'not_available') showStemsUnavailable()
      else if (data.status === 'done') renderStems(data.stems)
      else if (stemsContainer) {
        stemsContainer.innerHTML = '<p style="color:var(--muted);font-size:11px;">No stems yet.</p>'
      }
    } catch (_) {}
  }

  // Stems are not part of this build yet — a calm note instead of splitting errors
  function showStemsUnavailable() {
    if (stemsContainer) {
      stemsContainer.innerHTML =
        '<p style="color:var(--muted);font-size:11px;margin:8px 0 0;">Stems aren\'t included in this build yet.</p>'
    }
    if (splitStemsBtn) {
      splitStemsBtn.textContent = 'Not available'
      splitStemsBtn.disabled    = true
    }
  }

  function renderStems(stems) {
    if (!stemsContainer) return

    // Destroy any previous stem wavesurfer instances
    Object.values(stemWaveSurfers).forEach(sw => { try { sw.destroy() } catch (_) {} })
    stemWaveSurfers = {}
    stemsContainer.innerHTML = ''

    // ── Action bar: Open Folder + Save Stems ─────────────────────
    const bar = document.createElement('div')
    bar.style.cssText = 'display:flex;gap:6px;margin-bottom:8px;'
    bar.innerHTML = `
      <button id="stems-open-btn" class="ctrl-btn" style="font-size:11px;" onclick="revealStemsFolder()">Open Folder</button>
      <button id="stems-save-btn" class="ctrl-btn" style="font-size:11px;" onclick="saveStems()">Save Stems…</button>
    `
    stemsContainer.appendChild(bar)

    // ── One row per stem channel ──────────────────────────────────
    const channels = ['drums', 'bass', 'vocals', 'other']
    channels.forEach((ch, i) => {
      if (!stems[ch]) return
      const row     = document.createElement('div')
      row.className = 'stem-row'
      const waveId  = `stem-wave-${ch}`
      const playId  = `stem-play-${ch}`
      const exportId = `stem-export-${ch}`
      row.innerHTML = `
        <span class="stem-label">${ch}</span>
        <button id="${playId}" onclick="stemPlay('${ch}')">▶</button>
        <div id="${waveId}" class="stem-wave"></div>
        <button id="${exportId}" title="Export this stem (in/out region) as WAV">Export</button>
      `
      stemsContainer.appendChild(row)

      // Attach inject handler via data attribute to avoid apostrophe issues in path strings
      const exportBtn = document.getElementById(exportId)
      if (exportBtn) {
        exportBtn.addEventListener('click', () => window.exportClip(ch))
      }

      try {
        const sw = WaveSurfer.create({
          container:     `#${waveId}`,
          waveColor:     '#3a3a3a',
          progressColor: '#e47a2a',
          height:        36,
          barWidth:      2,
          interact:      true,
        })
        sw.load(`/audio/stream?path=${encodeURIComponent(stems[ch])}`)
        sw.on('play',   () => { const b = document.getElementById(playId); if (b) b.textContent = '⏸' })
        sw.on('pause',  () => { const b = document.getElementById(playId); if (b) b.textContent = '▶' })
        sw.on('finish', () => { const b = document.getElementById(playId); if (b) b.textContent = '▶' })
        stemWaveSurfers[ch] = sw
      } catch (_) {}
    })
  }

  /* ── Global controls (called from inline HTML) ───────────────── */
  window.playerPlay = () => {
    if (!ws) return
    ws.playPause()
  }

  window.playerStop = () => {
    if (!ws) return
    ws.stop()
    isPlaying = false
    jklSpeed  = 0
    setPlayIcon(false)
  }

  window.toggleLoop = () => {
    loopRegion = !loopRegion
    const btn = document.getElementById('loop-btn')
    if (btn) btn.classList.toggle('active', loopRegion)
  }

  window.copyPath = () => {
    if (!currentSound) return
    navigator.clipboard.writeText(currentSound.path).then(() => {
      const btn = document.getElementById('copy-path-btn')
      if (!btn) return
      btn.classList.add('copied')
      btn.setAttribute('data-copied', '1')
      clearTimeout(btn._copyTimer)
      btn._copyTimer = setTimeout(() => {
        btn.classList.remove('copied')
        btn.removeAttribute('data-copied')
      }, 1500)
    }).catch(() => {})
  }

  window.revealInFinder = async () => {
    if (!currentSound) return
    try {
      await fetch('/api/reveal', { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify({kind: 'sound', id: currentSound.id}) })
    } catch(_) {}
  }

  let _toastTimer = null
  const _exportResults = {}   // sound id -> last export response (session only)

  function showExportError (msg, soundId) {
    if (!statusToast) return
    if (soundId != null && (!currentSound || currentSound.id !== soundId)) return
    statusToast.textContent = '✗ ' + msg
    statusToast.style.color = '#d09090'
    clearTimeout(_toastTimer)
    _toastTimer = setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 5000)
  }

  function renderExportResult (soundId) {
    const box = document.getElementById('export-result')
    if (!box) return
    box.innerHTML = ''
    const data = _exportResults[soundId]
    if (data) showExportResult(data, soundId)
  }

  function showExportResult (data, soundId) {
    const box = document.getElementById('export-result')
    if (!box) return
    if (soundId != null) {
      _exportResults[soundId] = data
      if (!currentSound || currentSound.id !== soundId) return
    }
    box.innerHTML = ''
    const name = document.createElement('span')
    name.className = 'export-result-path'
    name.textContent = '✓ ' + data.filename
    name.title = data.path
    name.dataset.path = data.path
    const info = document.createElement('span')
    info.className = 'export-result-info'
    info.textContent = `${(data.duration_ms / 1000).toFixed(2)}s · ${data.sample_rate / 1000} kHz · ${data.bits}-bit`
    const reveal = document.createElement('button')
    reveal.className = 'ctrl-btn'
    reveal.textContent = 'Reveal'
    reveal.onclick = () => window.revealExport(data.path)
    box.append(name, info, reveal)
  }

  window.revealExport = async (path) => {
    try {
      const r = await fetch('/api/export/reveal', {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ path }),
      })
      if (!r.ok) showExportError(await r.text() || r.statusText)
    } catch (_) { showExportError('Network error') }
  }

  // Export the in/out region (whole file when no markers are set) as a lossless WAV.
  // `stem` (optional): drums|bass|vocals|other — export that stem instead of the source.
  window.exportClip = async (stem) => {
    if (!currentSound || !ws || !wsReady) return
    const soundId = currentSound.id
    const dur = ws.getDuration() * 1000
    const payload = { format: 'wav' }
    if (inPoint > 0 || outPoint < 1) {
      payload.start_ms = Math.round(inPoint * dur)
      payload.end_ms   = Math.round(outPoint * dur)
    }
    const fx = (window.getActiveFX && window.getActiveFX() !== 'none') ? window.getActiveFX() : null
    if (fx && !stem) payload.fx_preset = fx
    if (typeof stem === 'string') payload.stem = stem
    const btn = document.getElementById('export-btn')
    if (btn) btn.disabled = true
    try {
      const r = await fetch(`/api/sounds/${soundId}/export`, {
        method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(payload),
      })
      if (!r.ok) {
        const msg = (await r.text().catch(() => '')) || r.statusText
        showExportError(r.status === 404 && stem ? 'Stem not available yet' : msg, soundId)
        return
      }
      showExportResult(await r.json(), soundId)
    } catch (_) {
      showExportError('Network error', soundId)
    } finally {
      if (btn && currentSound && currentSound.id === soundId && wsReady) btn.disabled = false
    }
  }

  window.splitStems = async () => {
    if (!currentSound || !splitStemsBtn) return

    // Capture the sound ID and button at call time so poll callbacks
    // are not affected by subsequent sound selections.
    const soundId = currentSound.id
    const btn     = splitStemsBtn

    btn.textContent = 'Splitting...'
    btn.disabled    = true
    if (statusToast) { statusToast.textContent = '' }

    // Wrap the initial request so a network/server error is surfaced
    try {
      const r = await fetch(`/api/stems/${soundId}/split`, { method: 'POST' })
      const body = await r.json().catch(() => null)
      if (body && body.status === 'not_available') { showStemsUnavailable(); return }
      if (!r.ok) throw new Error(r.statusText)
    } catch (err) {
      btn.textContent = 'Split Stems'
      btn.disabled    = false
      if (statusToast) {
        statusToast.textContent = '✗ Failed to start splitting: ' + err.message
        statusToast.style.color = '#d09090'
        setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 6000)
      }
      return
    }

    // Guard: if the user switched sounds during the POST await, do not start timers
    if (!currentSound || currentSound.id !== soundId) return

    stemPollTimeout = setTimeout(() => {
      if (stemPollInterval) { clearInterval(stemPollInterval); stemPollInterval = null }
      stemPollTimeout = null
      // Guard: only mutate the button if this sound is still selected
      if (!currentSound || currentSound.id !== soundId) return
      btn.textContent = 'Split Stems'
      btn.disabled    = false
      if (statusToast) {
        statusToast.textContent  = '✗ Stem splitting timed out (>10 min)'
        statusToast.style.color  = '#d09090'
        setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 6000)
      }
    }, 10 * 60 * 1000)

    stemPollInterval = setInterval(async () => {
      // Guard: if the user switched sounds, stop polling immediately
      if (!currentSound || currentSound.id !== soundId) {
        clearInterval(stemPollInterval); stemPollInterval = null
        clearTimeout(stemPollTimeout);  stemPollTimeout  = null
        return
      }
      try {
        const r    = await fetch(`/api/stems/${soundId}`)
        const data = await r.json()
        // Re-check after the await in case the sound changed during the fetch
        if (!currentSound || currentSound.id !== soundId) {
          clearInterval(stemPollInterval); stemPollInterval = null
          clearTimeout(stemPollTimeout);  stemPollTimeout  = null
          return
        }
        if (data.status === 'done') {
          clearInterval(stemPollInterval); stemPollInterval = null
          clearTimeout(stemPollTimeout);  stemPollTimeout  = null
          btn.textContent = 'Stems Ready'
          btn.disabled    = false
          renderStems(data.stems)
        } else if (data.status === 'error') {
          clearInterval(stemPollInterval); stemPollInterval = null
          clearTimeout(stemPollTimeout);  stemPollTimeout  = null
          btn.textContent = 'Split Stems'
          btn.disabled    = false
          if (statusToast) {
            statusToast.textContent = '✗ ' + (data.error || 'Demucs failed')
            statusToast.style.color = '#d09090'
            setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 8000)
          }
          console.error('[stems] Demucs error:', data.error)
        }
        // 'processing' or 'not_started' → keep polling
      } catch (_) {
        clearInterval(stemPollInterval); stemPollInterval = null
        clearTimeout(stemPollTimeout);  stemPollTimeout  = null
      }
    }, 2000)
  }

  window.stemPlay = (ch) => {
    const sw = stemWaveSurfers[ch]
    if (sw) sw.playPause()
  }

  window.revealStemsFolder = async () => {
    if (!currentSound) return
    try {
      const r = await fetch(`/api/stems/${currentSound.id}/reveal`, { method: 'POST' })
      if (!r.ok) throw new Error(r.statusText)
    } catch (err) {
      if (statusToast) {
        statusToast.textContent = '✗ Could not open folder'
        statusToast.style.color = '#d09090'
        setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 3000)
      }
    }
  }

  window.saveStems = async () => {
    if (!currentSound) return
    const btn = document.getElementById('stems-save-btn')
    if (btn) { btn.textContent = 'Saving…'; btn.disabled = true }
    try {
      const r    = await fetch(`/api/stems/${currentSound.id}/save`, { method: 'POST' })
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      if (btn) { btn.textContent = 'Save Stems…'; btn.disabled = false }
      if (data.success) {
        if (statusToast) {
          statusToast.textContent = '✓ Saved to ' + data.path
          statusToast.style.color = '#90d090'
          setTimeout(() => { statusToast.textContent = '' }, 6000)
        }
      } else {
        if (statusToast) {
          statusToast.textContent = '✗ ' + (data.error || 'Save failed')
          statusToast.style.color = '#d09090'
          setTimeout(() => { statusToast.textContent = '' }, 5000)
        }
      }
    } catch (err) {
      if (btn) { btn.textContent = 'Save Stems…'; btn.disabled = false }
      if (statusToast) {
        statusToast.textContent = '✗ Save failed: ' + (err.message || 'Server error')
        statusToast.style.color = '#d09090'
        setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 5000)
      }
    }
  }

  /* ── Volume control ──────────────────────────────────────────────── */
  let _volume = 0.8
  let _muted  = false
  let _volBeforeMute = 0.8

  window.setVolume = (val) => {
    _volume = parseFloat(val)
    _muted  = _volume === 0
    _applyVolume()
    const pct = document.getElementById('volume-pct')
    if (pct) pct.textContent = Math.round(_volume * 100) + '%'
    const slider = document.getElementById('volume-slider')
    if (slider) slider.value = _volume
    _updateVolIcon()
  }

  window.toggleMute = () => {
    if (_muted) {
      _volume = _volBeforeMute || 0.8
      _muted  = false
    } else {
      _volBeforeMute = _volume
      _volume = 0
      _muted  = true
    }
    _applyVolume()
    const slider = document.getElementById('volume-slider')
    if (slider) slider.value = _volume
    const pct = document.getElementById('volume-pct')
    if (pct) pct.textContent = Math.round(_volume * 100) + '%'
    _updateVolIcon()
  }

  function _applyVolume() {
    if (ws) {
      try { ws.setVolume(_volume) } catch (_) {}
    }
    // Also drive the Web Audio gainOut if active
    if (_fxNodes && _fxNodes.gainOut) {
      _fxNodes.gainOut.gain.setTargetAtTime(_volume, _fxNodes.gainOut.context.currentTime, 0.02)
    }
  }

  function _updateVolIcon() {
    const wave1 = document.getElementById('vol-wave1')
    const wave2 = document.getElementById('vol-wave2')
    if (!wave1 || !wave2) return
    if (_muted || _volume === 0) {
      wave1.style.display = 'none'
      wave2.style.display = 'none'
    } else if (_volume < 0.5) {
      wave1.style.display = 'none'
      wave2.style.display = ''
    } else {
      wave1.style.display = ''
      wave2.style.display = ''
    }
  }

  window.refreshSuggestedTags = async () => {
    if (!currentSound) return
    if (suggestionsBlocked()) { DaouchiModels.openPanel(); return }
    const btn = document.getElementById('refresh-tags-btn')
    const sound = currentSound
    if (btn) { btn.textContent = '↻'; btn.disabled = true }
    showSuggestedTagsLoading()
    try {
      const r = await fetch(`/api/sounds/${sound.id}/suggested_tags`, { method: 'DELETE' })
      if (!r.ok) throw new Error(r.statusText)
      pollSuggestedTags(sound.id, sound)
    } catch (_) {
      if (playerSuggestedTagsEl)
        playerSuggestedTagsEl.innerHTML =
          '<span style="font-size:11px;color:#d09090;">Failed to refresh</span>'
    } finally {
      if (btn) { btn.textContent = '↺ Refresh'; btn.disabled = false }
    }
  }

  window.confirmTag = async (soundId, tag) => {
    try {
      const r = await fetch(`/api/sounds/${soundId}/suggested_tags/${encodeURIComponent(tag)}/confirm`, {
        method: 'POST',
      })
      if (!r.ok) {
        if (statusToast) {
          statusToast.textContent = '✗ Failed to confirm tag'
          statusToast.style.color = '#d09090'
          setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 3000)
        }
        return
      }
      if (currentSound && currentSound.id === soundId) {
        if (!currentSound.tags.includes(tag)) currentSound.tags.push(tag)
        // Re-render only the confirmed tags list — no need to re-fetch suggestions
        renderConfirmedTags(currentSound.tags, soundId)
        // Add 'just-confirmed' micro-animation to the newly confirmed chip
        if (playerTagsEl) {
          const chips = playerTagsEl.querySelectorAll('.tag-chip')
          chips.forEach(chip => {
            if (chip.textContent.trim().startsWith(tag)) {
              chip.classList.add('just-confirmed')
              setTimeout(() => chip.classList.remove('just-confirmed'), 200)
            }
          })
        }
        // Remove the confirmed tag from the suggestions panel without re-fetching
        if (playerSuggestedTagsEl) {
          playerSuggestedTagsEl.querySelectorAll('.tag-suggested').forEach(el => {
            if (el.textContent.trim().startsWith(tag)) el.remove()
          })
        }
      }
    } catch (_) {
      if (statusToast) {
        statusToast.textContent = '✗ Failed to confirm tag'
        statusToast.style.color = '#d09090'
        setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 3000)
      }
    }
  }

  window.removeTag = async (soundId, tag) => {
    // Optimistically remove and re-render; roll back on failure
    const soundAtCall = currentSound && currentSound.id === soundId ? currentSound : null
    const prevTags = soundAtCall ? soundAtCall.tags.slice() : null
    if (soundAtCall) {
      soundAtCall.tags = soundAtCall.tags.filter(t => t !== tag)
      renderConfirmedTags(soundAtCall.tags, soundId)
    }
    try {
      const r = await fetch(`/api/sounds/${soundId}/tags`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ remove: [tag] }),
      })
      if (!r.ok) throw new Error(r.statusText)
    } catch (_) {
      // Roll back optimistic removal
      if (soundAtCall && currentSound && currentSound.id === soundId) {
        soundAtCall.tags = prevTags
        renderConfirmedTags(soundAtCall.tags, soundId)
        if (statusToast) {
          statusToast.textContent = '✗ Failed to remove tag'
          statusToast.style.color = '#d09090'
          setTimeout(() => { if (statusToast) statusToast.textContent = '' }, 3000)
        }
      }
    }
  }

  /* ── J-K-L NLE playback engine ──────────────────────────────── */
  // L: forward 1× → 2× → 4× (cycle on repeated taps while playing forward)
  // J: reverse 1× → 2× → 4× (cycle on repeated taps while playing reverse)
  // K: stop / pause at current position
  function _getMedia() {
    if (!ws) return null
    // WaveSurfer v7: getMediaElement() or ws.media (internal)
    if (typeof ws.getMediaElement === 'function') return ws.getMediaElement()
    if (ws.media) return ws.media
    return null
  }

  function jklApply() {
    if (!ws || !wsReady) return
    if (jklSpeed === 0) {
      stopReversePlayback()
      try { ws.pause() } catch(_) {}
      return
    }
    const media = _getMedia()
    if (!media) return

    const speed   = Math.abs(jklSpeed)
    const forward = jklSpeed > 0

    if (!forward) {
      if (isPlaying) { try { ws.pause() } catch(_) {} }
      media.playbackRate = 1
      startReversePlayback(speed)
    } else {
      stopReversePlayback()
      media.playbackRate = speed
      try { ws.play() } catch(_) {}
    }
  }

  let _reverseRafId  = null
  let _reverseLast   = null
  let _reverseSpeed  = 1

  function startReversePlayback(speed) {
    stopReversePlayback()
    _reverseSpeed = speed
    _reverseLast  = performance.now()
    function step(now) {
      if (!ws || !wsReady || jklSpeed >= 0) { stopReversePlayback(); return }
      const dt    = (now - _reverseLast) / 1000
      _reverseLast = now
      const cur   = ws.getCurrentTime()
      const next  = Math.max(0, cur - dt * _reverseSpeed)
      ws.setTime(next)
      if (next <= 0) { stopReversePlayback(); jklSpeed = 0; setPlayIcon(false); return }
      _reverseRafId = requestAnimationFrame(step)
    }
    _reverseRafId = requestAnimationFrame(step)
  }

  function stopReversePlayback() {
    if (_reverseRafId !== null) { cancelAnimationFrame(_reverseRafId); _reverseRafId = null }
    _reverseLast = null
    // Restore playback rate to 1× when stopping reverse
    const media = ws && ws.getMediaElement ? ws.getMediaElement() : null
    if (media) media.playbackRate = 1
  }

  function showJklIndicator(label) {
    let el = document.getElementById('jkl-indicator')
    if (!el) {
      el = document.createElement('div')
      el.id = 'jkl-indicator'
      el.style.cssText = 'position:fixed;bottom:24px;right:24px;background:rgba(0,0,0,.8);color:#e47a2a;' +
        'font-size:22px;font-weight:700;padding:6px 16px;border-radius:8px;pointer-events:none;z-index:9999;opacity:0;transition:opacity .25s;'
      document.body.appendChild(el)
    }
    el.textContent = label
    el.style.opacity = '1'
    clearTimeout(el._t)
    el._t = setTimeout(() => { el.style.opacity = '0' }, 700)
  }

  /* ── Player tag input: Enter key ─────────────────────────────── */
  document.addEventListener('keydown', (e) => {
    const active = document.activeElement
    if (active && active.id === 'player-tag-input' && e.key === 'Enter') {
      e.preventDefault()
      window.addTagFromInput()
      return
    }

    // Guard: don't fire any player shortcuts when an input/textarea/select is focused
    if (active && (active.tagName === 'INPUT' || active.tagName === 'TEXTAREA' || active.tagName === 'SELECT')) return

    // ── J-K-L NLE shortcuts ──────────────────────────────────────
    if (e.code === 'KeyK' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      e.preventDefault()
      stopReversePlayback()
      jklSpeed = 0
      if (ws && wsReady) { try { ws.pause() } catch(_){} }
      const media = ws && ws.getMediaElement ? ws.getMediaElement() : null
      if (media) media.playbackRate = 1
      setPlayIcon(false)
      return
    }

    if (e.code === 'KeyL' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      e.preventDefault()
      stopReversePlayback()
      if (jklSpeed <= 0) {
        jklSpeed = 1          // was stopped or reverse → start 1×
      } else if (jklSpeed === 1) {
        jklSpeed = 2
      } else if (jklSpeed === 2) {
        jklSpeed = 4
      } else {
        jklSpeed = 1          // wrap back to 1× after 4×
      }
      jklApply()
      showJklIndicator('+' + jklSpeed + '×')
      return
    }

    if (e.code === 'KeyJ' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      e.preventDefault()
      if (jklSpeed >= 0) {
        jklSpeed = -1         // was stopped or forward → start reverse 1×
      } else if (jklSpeed === -1) {
        jklSpeed = -2
      } else if (jklSpeed === -2) {
        jklSpeed = -4
      } else {
        jklSpeed = -1
      }
      jklApply()
      showJklIndicator(jklSpeed + '×')
      return
    }

    // ── I / O  — set in/out points ───────────────────────────────
    if (e.code === 'KeyI' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      if (ws && wsReady) {
        const frac = ws.getCurrentTime() / ws.getDuration()
        inPoint = Math.max(0, Math.min(frac, outPoint - 0.01))
        updateMarkers()
      }
      return
    }
    if (e.code === 'KeyI' && !e.metaKey && !e.ctrlKey && !e.altKey && e.shiftKey) {
      // Shift+I: snap playhead to in-point
      if (ws && wsReady) ws.seekTo(inPoint)
      return
    }
    if (e.code === 'KeyO' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) {
      if (ws && wsReady) {
        const frac = ws.getCurrentTime() / ws.getDuration()
        outPoint = Math.max(inPoint + 0.01, Math.min(1, frac))
        updateMarkers()
      }
      return
    }
    if (e.code === 'KeyO' && !e.metaKey && !e.ctrlKey && !e.altKey && e.shiftKey) {
      // Shift+O: snap playhead to out-point
      if (ws && wsReady) ws.seekTo(outPoint)
      return
    }

    // Option+X (Alt+X): clear both markers
    if (e.code === 'KeyX' && e.altKey && !e.metaKey && !e.ctrlKey) {
      e.preventDefault()
      inPoint  = 0.0
      outPoint = 1.0
      updateMarkers()
      return
    }

    if (e.code === 'Enter' && !e.metaKey && !e.ctrlKey) {
      if (ws && wsReady && currentSound) {
        window.exportClip()
      }
      return
    }

    if (e.code === 'Space') {
      e.preventDefault()
      // Space = play/pause; also reset JKL reverse if active
      stopReversePlayback()
      jklSpeed = 0
      window.playerPlay()
      return
    }
    if (e.code === 'ArrowUp' || e.code === 'ArrowDown') {
      e.preventDefault()
      document.dispatchEvent(
        new CustomEvent('navigate-results', { detail: e.code === 'ArrowDown' ? 1 : -1 })
      )
    }
  })

  /* ── Utility ─────────────────────────────────────────────────── */
  function formatTime(s) {
    if (!s && s !== 0) return '—'
    const sec = Math.floor(s)
    const m   = Math.floor(sec / 60)
    return m + ':' + String(sec % 60).padStart(2, '0')
  }

  function escHtml(str) {
    return String(str)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;')
  }

  // Produces a JS string literal safe to embed inside an HTML attribute value.
  // e.g. escAttr("don't") → "'don\\'t'" so onclick="fn(id,'don\'t')" is valid.
  function escAttr(str) {
    return "'" + String(str).replace(/\\/g, '\\\\').replace(/'/g, "\\'") + "'"
  }

  /* ── Audio FX Engine ────────────────────────────────────────── */
  let _audioCtx    = null
  let _fxSource    = null
  let _fxNodes     = null   // { filter, dryGain, convolver, wetGain, tremoloGain, lfoOsc, gainOut }
  let _activeFX    = 'none'
  let _clubDropInterval = null

  function _teardownAudioChain() {
    if (_clubDropInterval) { clearInterval(_clubDropInterval); _clubDropInterval = null }
    if (_fxNodes && _fxNodes.lfoOsc) {
      try { _fxNodes.lfoOsc.stop() } catch (_) {}
    }
    if (_fxSource) { try { _fxSource.disconnect() } catch (_) {} }
    _fxSource = null
    _fxNodes  = null
  }

  function _buildReverbIR(ctx, duration, decay) {
    const sr     = ctx.sampleRate
    const length = Math.ceil(sr * duration)
    const ir     = ctx.createBuffer(2, length, sr)
    for (let ch = 0; ch < 2; ch++) {
      const d = ir.getChannelData(ch)
      for (let i = 0; i < length; i++) {
        d[i] = (Math.random() * 2 - 1) * Math.pow(1 - i / length, decay)
      }
    }
    return ir
  }

  function setupAudioChain(mediaEl) {
    _teardownAudioChain()
    if (!mediaEl) return

    _audioCtx = _audioCtx || new (window.AudioContext || window.webkitAudioContext)()
    if (_audioCtx.state === 'suspended') _audioCtx.resume().catch(() => {})

    try {
      _fxSource = _audioCtx.createMediaElementSource(mediaEl)
    } catch (_) {
      // Already connected (MediaElementSource can only be created once per element)
      return
    }

    // Main biquad filter node
    const filter = _audioCtx.createBiquadFilter()
    filter.type            = 'lowpass'
    filter.frequency.value = 20000

    // Dry / wet split for reverb
    const dryGain = _audioCtx.createGain()
    const wetGain = _audioCtx.createGain()
    dryGain.gain.value = 1
    wetGain.gain.value = 0

    // Convolver with synthetic hall impulse response
    const convolver = _audioCtx.createConvolver()
    convolver.buffer = _buildReverbIR(_audioCtx, 3.5, 3.0)

    // Tremolo for stutter: LFO square wave drives a GainNode
    const tremoloGain = _audioCtx.createGain()
    tremoloGain.gain.value = 1
    const lfoOsc = _audioCtx.createOscillator()
    lfoOsc.type            = 'square'
    lfoOsc.frequency.value = 10   // 10 Hz ≈ 1/16th at 160bpm
    const lfoGain = _audioCtx.createGain()
    lfoGain.gain.value = 0.5     // oscillates tremoloGain.gain ±0.5 around base 0.55
    tremoloGain.gain.setValueAtTime(0.55, _audioCtx.currentTime)
    lfoOsc.connect(lfoGain)
    lfoGain.connect(tremoloGain.gain)
    // LFO starts stopped; we start it only when stutter is active

    // WaveShaper for lo-fi distortion
    const waveShaper = _audioCtx.createWaveShaper()
    const curve = new Float32Array(256)
    for (let i = 0; i < 256; i++) {
      const x = (i * 2) / 256 - 1
      curve[i] = ((Math.PI + 300) * x) / (Math.PI + 300 * Math.abs(x))
    }
    waveShaper.curve  = curve
    waveShaper.oversample = '4x'

    // Output gain
    const gainOut = _audioCtx.createGain()
    gainOut.gain.value = 1

    // Default routing: source → filter → dryGain → tremoloGain → gainOut → dest
    //                  source → filter → wetGain → convolver → gainOut (bypassed by default)
    _fxSource.connect(filter)
    filter.connect(dryGain)
    filter.connect(convolver)
    convolver.connect(wetGain)
    dryGain.connect(tremoloGain)
    wetGain.connect(tremoloGain)
    tremoloGain.connect(gainOut)
    gainOut.connect(_audioCtx.destination)

    _fxNodes = { filter, dryGain, wetGain, convolver, tremoloGain, lfoOsc, lfoGain, waveShaper, gainOut }
    applyFXPreset(_activeFX)
  }

  function applyFXPreset(preset) {
    _activeFX = preset
    if (!_fxNodes || !_audioCtx) return
    const { filter, dryGain, wetGain, tremoloGain, lfoOsc, lfoGain, waveShaper, gainOut } = _fxNodes

    // Stop club-drop sweep if running
    if (_clubDropInterval) { clearInterval(_clubDropInterval); _clubDropInterval = null }

    // Stop tremolo LFO
    try { lfoOsc.stop() } catch (_) {}
    // Recreate LFO (stop is final, can't restart)
    const newLfo = _audioCtx.createOscillator()
    newLfo.type            = lfoOsc.type
    newLfo.frequency.value = lfoOsc.frequency.value
    newLfo.connect(lfoGain)
    _fxNodes.lfoOsc = newLfo

    // Reconnect waveShaper only when needed
    try { filter.disconnect(waveShaper) } catch (_) {}
    try { waveShaper.disconnect(dryGain) } catch (_) {}
    try { filter.connect(dryGain) } catch (_) {}

    const now = _audioCtx.currentTime

    switch (preset) {
      case 'underwater':
        filter.type            = 'lowpass'
        filter.frequency.setTargetAtTime(450,   now, 0.05)
        filter.Q.setTargetAtTime(1.2,            now, 0.05)
        dryGain.gain.setTargetAtTime(1,          now, 0.05)
        wetGain.gain.setTargetAtTime(0,          now, 0.05)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.05)
        gainOut.gain.setTargetAtTime(1.4,        now, 0.05)   // boost since bass-only
        break

      case 'telephone':
        filter.type            = 'bandpass'
        filter.frequency.setTargetAtTime(1500,  now, 0.05)
        filter.Q.setTargetAtTime(3.5,            now, 0.05)
        dryGain.gain.setTargetAtTime(1,          now, 0.05)
        wetGain.gain.setTargetAtTime(0,          now, 0.05)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.05)
        gainOut.gain.setTargetAtTime(1.8,        now, 0.05)
        break

      case 'clubdrop': {
        filter.type            = 'lowpass'
        filter.frequency.setTargetAtTime(20000, now, 0.02)
        filter.Q.setTargetAtTime(1,              now, 0.02)
        dryGain.gain.setTargetAtTime(1,          now, 0.02)
        wetGain.gain.setTargetAtTime(0,          now, 0.02)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.02)
        gainOut.gain.setTargetAtTime(1,          now, 0.02)
        // Sweep: 20kHz → 300Hz over 3 seconds using repeated setTargetAtTime
        const startFreq = 20000, endFreq = 300, sweepMs = 3000
        const steps = 60, stepMs = sweepMs / steps
        let step = 0
        _clubDropInterval = setInterval(() => {
          if (!_fxNodes || _activeFX !== 'clubdrop') { clearInterval(_clubDropInterval); return }
          step++
          const t = step / steps  // 0..1
          const freq = startFreq * Math.pow(endFreq / startFreq, t)
          filter.frequency.setTargetAtTime(freq, _audioCtx.currentTime, 0.01)
          if (step >= steps) clearInterval(_clubDropInterval)
        }, stepMs)
        break
      }

      case 'reverb':
        filter.type            = 'allpass'
        filter.frequency.setTargetAtTime(1000,  now, 0.05)
        dryGain.gain.setTargetAtTime(0.4,        now, 0.08)
        wetGain.gain.setTargetAtTime(0.6,        now, 0.08)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.05)
        gainOut.gain.setTargetAtTime(1,          now, 0.05)
        break

      case 'lofi':
        // Soft lowpass to simulate old tape, then waveshaper distortion
        try { filter.disconnect(dryGain) } catch (_) {}
        filter.type            = 'lowpass'
        filter.frequency.setTargetAtTime(8000,  now, 0.05)
        filter.Q.setTargetAtTime(0.5,            now, 0.05)
        dryGain.gain.setTargetAtTime(0,          now, 0.01)
        // Route through waveShaper
        filter.connect(waveShaper)
        waveShaper.connect(dryGain)
        dryGain.gain.setTargetAtTime(1,          now, 0.05)
        wetGain.gain.setTargetAtTime(0,          now, 0.05)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.05)
        gainOut.gain.setTargetAtTime(0.9,        now, 0.05)
        break

      case 'stutter':
        filter.type            = 'allpass'
        filter.frequency.setTargetAtTime(1000,  now, 0.02)
        dryGain.gain.setTargetAtTime(1,          now, 0.02)
        wetGain.gain.setTargetAtTime(0,          now, 0.02)
        gainOut.gain.setTargetAtTime(1,          now, 0.02)
        // Start LFO tremolo
        tremoloGain.gain.cancelScheduledValues(now)
        tremoloGain.gain.setValueAtTime(0.55,    now)
        _fxNodes.lfoOsc.start()
        break

      default: // 'none'
        filter.type            = 'lowpass'
        filter.frequency.setTargetAtTime(20000, now, 0.05)
        filter.Q.setTargetAtTime(1,              now, 0.05)
        dryGain.gain.setTargetAtTime(1,          now, 0.05)
        wetGain.gain.setTargetAtTime(0,          now, 0.05)
        tremoloGain.gain.cancelScheduledValues(now)
        tremoloGain.gain.setTargetAtTime(1,      now, 0.05)
        gainOut.gain.setTargetAtTime(1,          now, 0.05)
        break
    }
  }

  // Listen for FX preset changes dispatched by Alpine
  document.addEventListener('fx-preset-changed', (e) => {
    _activeFX = e.detail
    if (_fxNodes) applyFXPreset(e.detail)
    // Resume AudioContext if suspended (browser autoplay policy)
    if (_audioCtx && _audioCtx.state === 'suspended') _audioCtx.resume().catch(() => {})
  })

  // Expose globally so the export call can read the active FX
  window.getActiveFX = () => _activeFX
})()
