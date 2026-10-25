;(function () {
  'use strict'

  let points = []
  let pointsStale = false
  let canvas, ctx
  let logicalW = 0, logicalH = 0
  let scale = { minX: 0, maxX: 1, minY: 0, maxY: 1 }
  let activeFolder = null  // kept for back-compat event emitter
  let activeFilter = new Set()  // multi-select: empty = show all
  let selectedSoundId = null

  // Tooltip state
  let tooltipHovered = null       // { cx, cy, p } of hovered dot
  let tooltipVisible = null       // same, but only set after dwell delay
  let tooltipTimer   = null       // setTimeout handle

  // Lasso state
  let lassoActive    = false
  let lassoPath      = []
  let lassoStart     = null       // { x, y } canvas-logical coords of mousedown
  let lassoDragged   = false      // true once mouse moves > 5px from start
  let lassoSelected  = new Set()  // sound_ids currently lasso-selected

  // Pan state
  let panActive = false
  let panStart  = null  // { mx, my, panX, panY }

  // Viewport: zoom around canvas center, then pan
  let viewport = { panX: 0, panY: 0, zoom: 1 }

  // Context menu state
  let ctxMenu = null  // { cx, cy, p } — the dot for which menu is shown

  // Distinct palette
  const PALETTE = [
    '#4a9eff', '#e47a2a', '#7ec87e', '#d47ad4', '#f0c040',
    '#ff7070', '#40d4c8', '#e0a040', '#a07ce0', '#70d0a0',
    '#ff9eb0', '#90c0ff', '#c8e060', '#ffb060', '#80d8e0',
    '#d4a08c',
  ]
  const _collectionColors = {}
  let _colorIdx = 0

  function colorFor(collection) {
    const key = collection || 'unknown'
    if (!_collectionColors[key]) {
      _collectionColors[key] = PALETTE[_colorIdx % PALETTE.length]
      _colorIdx++
    }
    return _collectionColors[key]
  }

  /* ── Point-in-polygon (ray casting) ─────────────────────────── */
  function pointInPolygon(px, py, poly) {
    let inside = false
    for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
      const xi = poly[i].x, yi = poly[i].y
      const xj = poly[j].x, yj = poly[j].y
      if (((yi > py) !== (yj > py)) && px < ((xj - xi) * (py - yi) / (yj - yi) + xi)) {
        inside = !inside
      }
    }
    return inside
  }

  /* ── Dot radius by duration ──────────────────────────────────── */
  function dotRadius(duration, active) {
    if (!active) return 2
    const d = duration || 1
    if (d < 1)  return 3
    if (d < 5)  return 4
    if (d < 30) return 5
    return 7
  }

  /* ── Folder filter (from sidebar or external event) ─────────── */
  document.addEventListener('radar-folder-filter', (e) => {
    activeFolder = e.detail || null
    activeFilter = activeFolder ? new Set([activeFolder]) : new Set()
    if (canvas && points.length) render()
  })

  /* ── Cache invalidation ──────────────────────────────────────── */
  document.addEventListener('radar-invalidate', () => {
    pointsStale = true
    points = []
  })

  /* ── Track selected sound from outside ──────────────────────── */
  document.addEventListener('sound-selected', (e) => {
    const s = e.detail
    if (s && s.id != null) selectedSoundId = s.id
    else if (s && s.sound_id != null) selectedSoundId = s.sound_id
    if (canvas && points.length) {
      render()
      // Rebuild similarity strip for the newly selected sound
      const p = points.find(pt => pt.sound_id === selectedSoundId)
      if (p) buildSimilarityStrip(p)
    }
  })

  /* ── HiDPI canvas sizing ─────────────────────────────────────── */
  function sizeCanvas() {
    const rect = canvas.getBoundingClientRect()
    const dpr  = window.devicePixelRatio || 1
    logicalW = rect.width  || 500
    logicalH = rect.height || 400
    canvas.width  = logicalW * dpr
    canvas.height = logicalH * dpr
    ctx.scale(dpr, dpr)
  }

  /* ── Initialization ──────────────────────────────────────────── */
  document.addEventListener('show-radar', async () => {
    canvas = document.getElementById('radar-canvas')
    if (!canvas) return
    ctx = canvas.getContext('2d')

    sizeCanvas()

    if (!points.length) {
      pointsStale = false
      try {
        const r = await fetch('/api/radar/points')
        if (!r.ok) {
          console.warn('radar points fetch failed', r.status)
          ctx.fillStyle = '#d09090'
          ctx.font = '12px system-ui, sans-serif'
          ctx.fillText('Failed to load radar data (' + r.status + ')', PAD, logicalH / 2)
          return
        }
        const data = await r.json()
        points = data.points || []
      } catch (e) {
        console.warn('radar fetch error', e)
        points = []
      }

      if (points.length) {
        scale.minX = Math.min(...points.map(p => p.x))
        scale.maxX = Math.max(...points.map(p => p.x))
        scale.minY = Math.min(...points.map(p => p.y))
        scale.maxY = Math.max(...points.map(p => p.y))
      }

      if (scale.maxX === scale.minX) { scale.minX -= 0.5; scale.maxX += 0.5 }
      if (scale.maxY === scale.minY) { scale.minY -= 0.5; scale.maxY += 0.5 }
    }

    if (!points.length) {
      ctx.clearRect(0, 0, logicalW, logicalH)
      ctx.fillStyle = '#171717'
      ctx.fillRect(0, 0, logicalW, logicalH)
      ctx.fillStyle = '#555'
      ctx.font = '13px system-ui, sans-serif'
      ctx.textAlign = 'center'
      ctx.fillText('No acoustic data yet — click Generate to build the radar', logicalW / 2, logicalH / 2)
      ctx.textAlign = 'left'
      return
    }

    render()
    attachCanvasEvents()
    buildSidebar()
    attachResizeHandle()
  })

  /* ── Window resize ───────────────────────────────────────────── */
  window.addEventListener('resize', () => {
    if (!canvas) return
    sizeCanvas()
    if (points.length) render()
  })

  /* ── Coordinate mapping (uses logicalW/logicalH + viewport) ────── */
  const PAD = 32

  function toCanvas(x, y) {
    const w  = logicalW - PAD * 2
    const h  = logicalH - PAD * 2
    const bx = PAD + ((x - scale.minX) / (scale.maxX - scale.minX)) * w
    const by = PAD + (1 - (y - scale.minY) / (scale.maxY - scale.minY)) * h
    const cx = (bx - logicalW / 2) * viewport.zoom + logicalW / 2 + viewport.panX
    const cy = (by - logicalH / 2) * viewport.zoom + logicalH / 2 + viewport.panY
    return { cx, cy }
  }

  function fromCanvas(cx, cy) {
    const bx = (cx - viewport.panX - logicalW / 2) / viewport.zoom + logicalW / 2
    const by = (cy - viewport.panY - logicalH / 2) / viewport.zoom + logicalH / 2
    const w  = logicalW - PAD * 2
    const h  = logicalH - PAD * 2
    return {
      x: scale.minX + ((bx - PAD) / w) * (scale.maxX - scale.minX),
      y: scale.minY + (1 - (by - PAD) / h) * (scale.maxY - scale.minY),
    }
  }

  /* ── Render ──────────────────────────────────────────────────── */
  function render() {
    ctx.clearRect(0, 0, logicalW, logicalH)

    // Background
    ctx.fillStyle = '#171717'
    ctx.fillRect(0, 0, logicalW, logicalH)

    // Subtle grid
    ctx.strokeStyle = '#252525'
    ctx.lineWidth   = 1
    const gridLines = 6
    for (let i = 1; i < gridLines; i++) {
      const xLine = PAD + (i / gridLines) * (logicalW - PAD * 2)
      const yLine = PAD + (i / gridLines) * (logicalH - PAD * 2)
      ctx.beginPath(); ctx.moveTo(xLine, PAD); ctx.lineTo(xLine, logicalH - PAD); ctx.stroke()
      ctx.beginPath(); ctx.moveTo(PAD, yLine); ctx.lineTo(logicalW - PAD, yLine); ctx.stroke()
    }

    // Border
    ctx.strokeStyle = '#2e2e2e'
    ctx.lineWidth   = 1
    ctx.strokeRect(PAD, PAD, logicalW - PAD * 2, logicalH - PAD * 2)

    // Points
    points.forEach(p => {
      const { cx, cy } = toCanvas(p.x, p.y)
      const active = !activeFilter.size || activeFilter.has(p.collection)
      const inLasso = lassoSelected.has(p.sound_id)
      const isSelected = p.sound_id === selectedSoundId || p.id === selectedSoundId
      const radius = dotRadius(p.duration, active)

      // Selected dot: bright white ring
      if (isSelected) {
        ctx.beginPath()
        ctx.arc(cx, cy, radius + 3, 0, Math.PI * 2)
        ctx.strokeStyle = 'rgba(255,255,255,0.9)'
        ctx.lineWidth   = 1.5
        ctx.stroke()
      }

      // Lasso-selected: white ring
      if (inLasso && !isSelected) {
        ctx.beginPath()
        ctx.arc(cx, cy, radius + 2, 0, Math.PI * 2)
        ctx.strokeStyle = 'rgba(255,255,255,0.7)'
        ctx.lineWidth   = 2
        ctx.stroke()
      }

      ctx.beginPath()
      ctx.arc(cx, cy, inLasso ? radius + 1 : radius, 0, Math.PI * 2)
      ctx.fillStyle   = colorFor(p.collection)
      ctx.globalAlpha = active ? 0.88 : 0.12
      ctx.fill()
      ctx.globalAlpha = 1
    })

    // Legend
    const seen = [...new Set(points.map(p => p.collection || 'unknown'))]
    const LX = PAD + 4
    const LY = logicalH - PAD - (seen.length * 16) - 4
    ctx.font = '10px system-ui, sans-serif'
    seen.forEach((col, i) => {
      const y = LY + i * 16
      ctx.beginPath()
      ctx.arc(LX + 5, y + 5, 4, 0, Math.PI * 2)
      ctx.fillStyle   = colorFor(col)
      ctx.globalAlpha = 0.9
      ctx.fill()
      ctx.globalAlpha = 1
      ctx.fillStyle = '#aaaaaa'
      ctx.fillText(col, LX + 14, y + 9)
    })

    // Hover highlight ring (shown immediately, tooltip box is delayed)
    if (tooltipHovered) {
      const { cx, cy } = tooltipHovered
      ctx.beginPath()
      ctx.arc(cx, cy, 9, 0, Math.PI * 2)
      ctx.strokeStyle = '#ffffff'
      ctx.lineWidth   = 1.5
      ctx.globalAlpha = 0.8
      ctx.stroke()
      ctx.globalAlpha = 1
    }

    // Rich tooltip (after dwell delay)
    if (tooltipVisible) {
      drawRichTooltip(tooltipVisible)
    }

    // Lasso overlay
    if (lassoActive && lassoPath.length > 1) {
      ctx.save()
      ctx.beginPath()
      ctx.moveTo(lassoPath[0].x, lassoPath[0].y)
      for (let i = 1; i < lassoPath.length; i++) {
        ctx.lineTo(lassoPath[i].x, lassoPath[i].y)
      }
      ctx.strokeStyle = '#ff8c00'
      ctx.lineWidth   = 1.5
      ctx.setLineDash([4, 3])
      ctx.stroke()
      ctx.setLineDash([])
      ctx.restore()
    }

    // Context menu
    if (ctxMenu) {
      drawContextMenu(ctxMenu)
    }

    // Zoom indicator
    if (viewport.zoom > 1.05) {
      const zoomLabel = viewport.zoom.toFixed(1) + '×'
      ctx.font = '11px system-ui, sans-serif'
      ctx.fillStyle = 'rgba(228,122,42,0.85)'
      ctx.textAlign = 'right'
      ctx.fillText(zoomLabel + '  dbl-click to reset', logicalW - PAD, PAD - 8)
      ctx.textAlign = 'left'
    }
  }

  /* ── Rich tooltip ────────────────────────────────────────────── */
  function drawRichTooltip({ cx, cy, p }) {
    const TPAD   = 8
    const TPAD_H = 10
    const lineH  = 16
    const tags   = (p.tags || []).slice(0, 3)
    const label  = p.filename || String(p.sound_id)
    const sub    = [p.collection, p.duration != null ? fmtDur(p.duration) : null]
      .filter(Boolean).join(' · ')

    ctx.font = '600 12px system-ui, sans-serif'
    const labelW = ctx.measureText(label).width

    ctx.font = '10px system-ui, sans-serif'
    const subW = ctx.measureText(sub).width

    // Chip widths
    let chipsTotalW = 0
    const chipWidths = tags.map(t => {
      ctx.font = '9px system-ui, sans-serif'
      const tw = ctx.measureText(t).width + 8
      chipsTotalW += tw + 4
      return tw
    })

    const boxW = Math.max(labelW, subW, chipsTotalW) + TPAD_H * 2
    const rows = 1 + (sub ? 1 : 0) + (tags.length ? 1 : 0)
    const boxH = TPAD * 2 + rows * lineH

    // Flip logic
    let bx = cx + 12
    let by = cy + 12
    if (bx + boxW > logicalW - PAD) bx = cx - boxW - 12
    if (by + boxH > logicalH - PAD) by = cy - boxH - 12

    // Box
    ctx.save()
    ctx.fillStyle   = 'rgba(15,15,15,0.95)'
    ctx.strokeStyle = '#3a3a3a'
    ctx.lineWidth   = 1
    roundRect(ctx, bx, by, boxW, boxH, 6)
    ctx.fill()
    ctx.stroke()

    let ty = by + TPAD + 12

    // Filename
    ctx.font      = '600 12px system-ui, sans-serif'
    ctx.fillStyle = '#e8e8e8'
    ctx.fillText(label, bx + TPAD_H, ty)
    ty += lineH

    // Collection / duration
    if (sub) {
      ctx.font      = '10px system-ui, sans-serif'
      ctx.fillStyle = '#888888'
      ctx.fillText(sub, bx + TPAD_H, ty)
      ty += lineH
    }

    // Tag chips
    if (tags.length) {
      let tx = bx + TPAD_H
      tags.forEach((tag, i) => {
        const cw = chipWidths[i]
        ctx.fillStyle   = 'rgba(130,80,200,0.35)'
        ctx.strokeStyle = 'rgba(160,100,230,0.6)'
        ctx.lineWidth   = 0.5
        roundRect(ctx, tx, ty - 10, cw, 14, 3)
        ctx.fill()
        ctx.stroke()
        ctx.font      = '9px system-ui, sans-serif'
        ctx.fillStyle = '#c090f0'
        ctx.fillText(tag, tx + 4, ty)
        tx += cw + 4
      })
    }

    ctx.restore()
  }

  function fmtDur(s) {
    if (s < 60) return s.toFixed(1) + 's'
    return Math.floor(s / 60) + 'm' + String(Math.floor(s % 60)).padStart(2, '0') + 's'
  }

  function roundRect(ctx, x, y, w, h, r) {
    ctx.beginPath()
    ctx.moveTo(x + r, y)
    ctx.lineTo(x + w - r, y)
    ctx.quadraticCurveTo(x + w, y, x + w, y + r)
    ctx.lineTo(x + w, y + h - r)
    ctx.quadraticCurveTo(x + w, y + h, x + w - r, y + h)
    ctx.lineTo(x + r, y + h)
    ctx.quadraticCurveTo(x, y + h, x, y + h - r)
    ctx.lineTo(x, y + r)
    ctx.quadraticCurveTo(x, y, x + r, y)
    ctx.closePath()
  }

  /* ── Context menu ────────────────────────────────────────────── */
  const MENU_W = 160
  const MENU_ITEM_H = 28
  const MENU_ITEMS = [
    { label: '▶ Play sound',      action: 'play' },
    { label: '⟳ Find 20 similar', action: 'similar' },
  ]

  function drawContextMenu({ cx, cy, p }) {
    const boxH = MENU_ITEM_H * MENU_ITEMS.length + 8
    let mx = cx + 8
    let my = cy + 8
    if (mx + MENU_W > logicalW - 4) mx = cx - MENU_W - 8
    if (my + boxH > logicalH - 4)   my = cy - boxH - 8

    ctx.save()
    ctx.fillStyle   = 'rgba(20,20,20,0.97)'
    ctx.strokeStyle = '#3a3a3a'
    ctx.lineWidth   = 1
    roundRect(ctx, mx, my, MENU_W, boxH, 6)
    ctx.fill()
    ctx.stroke()

    MENU_ITEMS.forEach((item, i) => {
      const iy = my + 4 + i * MENU_ITEM_H
      ctx.font      = '12px system-ui, sans-serif'
      ctx.fillStyle = '#d4d4d4'
      ctx.fillText(item.label, mx + 10, iy + 18)
    })

    // Store for hit-testing
    ctxMenu._bounds = { mx, my, boxH }
    ctx.restore()
  }

  function menuItemAt(mx2, my2) {
    if (!ctxMenu || !ctxMenu._bounds) return null
    const { mx, my, boxH } = ctxMenu._bounds
    if (mx2 < mx || mx2 > mx + MENU_W || my2 < my || my2 > my + boxH) return null
    const idx = Math.floor((my2 - my - 4) / MENU_ITEM_H)
    if (idx < 0 || idx >= MENU_ITEMS.length) return null
    return MENU_ITEMS[idx]
  }

  /* ── Mouse helpers ───────────────────────────────────────────── */
  function getLogicalXY(e) {
    const rect = canvas.getBoundingClientRect()
    return {
      mx: e.clientX - rect.left,
      my: e.clientY - rect.top,
    }
  }

  function hitTest(mx, my, threshold = 12) {
    let closest = null, minDist = threshold
    points.forEach(p => {
      const { cx, cy } = toCanvas(p.x, p.y)
      const dist = Math.hypot(cx - mx, cy - my)
      if (dist < minDist) { minDist = dist; closest = { cx, cy, p } }
    })
    return closest
  }

  /* ── Canvas mouse events ─────────────────────────────────────── */
  function attachCanvasEvents() {
    if (!canvas._radarEventsAttached) {
      canvas.addEventListener('mousemove',  onMouseMove)
      canvas.addEventListener('mousedown',  onMouseDown)
      canvas.addEventListener('mouseup',    onMouseUp)
      canvas.addEventListener('mouseleave', onMouseLeave)
      canvas.addEventListener('click',      onCanvasClick)
      canvas.addEventListener('wheel',      onWheel, { passive: false })
      canvas.addEventListener('dblclick',   onDblClick)
      document.addEventListener('keydown',  onKeyDown)
      canvas._radarEventsAttached = true
    }
  }

  function onWheel(e) {
    e.preventDefault()
    const { mx, my } = getLogicalXY(e)
    const factor  = e.deltaY < 0 ? 1.18 : 1 / 1.18
    const newZoom = Math.max(1, Math.min(40, viewport.zoom * factor))
    if (newZoom === viewport.zoom) return
    const W = logicalW, H = logicalH
    // base coord under cursor (before viewport)
    const bx = (mx - viewport.panX - W / 2) / viewport.zoom + W / 2
    const by = (my - viewport.panY - H / 2) / viewport.zoom + H / 2
    // keep that base coord under cursor after zoom
    viewport.panX = mx - (bx - W / 2) * newZoom - W / 2
    viewport.panY = my - (by - H / 2) * newZoom - H / 2
    viewport.zoom = newZoom
    render()
  }

  function onDblClick() {
    viewport.panX = 0
    viewport.panY = 0
    viewport.zoom = 1
    render()
  }

  function onMouseMove(e) {
    if (!points.length) return
    const { mx, my } = getLogicalXY(e)

    // Pan drag
    if (panActive && panStart) {
      viewport.panX = panStart.panX + (mx - panStart.mx)
      viewport.panY = panStart.panY + (my - panStart.my)
      render()
      return
    }

    // Lasso drag
    if (lassoActive) {
      if (lassoStart) {
        const dx = mx - lassoStart.x
        const dy = my - lassoStart.y
        if (!lassoDragged && (Math.abs(dx) > 5 || Math.abs(dy) > 5)) {
          lassoDragged = true
        }
      }
      if (lassoDragged) {
        lassoPath.push({ x: mx, y: my })
        render()
        return
      }
    }

    // Tooltip dwell logic
    const hit = hitTest(mx, my)
    if (hit && hit.p.sound_id === (tooltipHovered && tooltipHovered.p.sound_id)) {
      // same dot, nothing to reset
    } else {
      tooltipHovered = hit
      tooltipVisible = null
      if (tooltipTimer) { clearTimeout(tooltipTimer); tooltipTimer = null }
      if (hit) {
        tooltipTimer = setTimeout(() => {
          tooltipVisible = tooltipHovered
          render()
        }, 150)
      }
    }

    render()
    if (hit) {
      canvas.style.cursor = 'pointer'
    } else if (panActive) {
      canvas.style.cursor = 'grabbing'
    } else if (viewport.zoom > 1) {
      canvas.style.cursor = 'grab'
    } else {
      canvas.style.cursor = 'crosshair'
    }
  }

  function onMouseDown(e) {
    if (!points.length) return
    const { mx, my } = getLogicalXY(e)

    // If context menu open, let click handler deal with it
    if (ctxMenu) return

    const hit = hitTest(mx, my)
    if (!hit) {
      if (viewport.zoom > 1) {
        // Pan mode when zoomed in
        panActive = true
        panStart  = { mx, my, panX: viewport.panX, panY: viewport.panY }
        canvas.style.cursor = 'grabbing'
      } else {
        // Start lasso on empty space at zoom=1
        lassoActive  = true
        lassoDragged = false
        lassoStart   = { x: mx, y: my }
        lassoPath    = [{ x: mx, y: my }]
      }
    }
  }

  function onMouseUp(e) {
    // End pan
    if (panActive) {
      panActive = false
      panStart  = null
      canvas.style.cursor = viewport.zoom > 1 ? 'grab' : 'crosshair'
      return
    }

    if (!lassoActive) return
    const { mx, my } = getLogicalXY(e)

    if (lassoDragged && lassoPath.length > 2) {
      // Close and test polygon
      lassoPath.push(lassoPath[0])
      lassoSelected = new Set()
      points.forEach(p => {
        const { cx, cy } = toCanvas(p.x, p.y)
        if (pointInPolygon(cx, cy, lassoPath)) {
          lassoSelected.add(p.sound_id)
        }
      })
      document.dispatchEvent(new CustomEvent('radar-lasso-select', {
        detail: [...lassoSelected],
      }))
    }
    // else: single click — handled by onClick

    lassoActive  = false
    lassoDragged = false
    lassoStart   = null
    lassoPath    = []
    render()
  }

  function onMouseLeave() {
    tooltipHovered = null
    tooltipVisible = null
    if (tooltipTimer) { clearTimeout(tooltipTimer); tooltipTimer = null }
    panActive    = false
    panStart     = null
    lassoActive  = false
    lassoDragged = false
    lassoPath    = []
    render()
    canvas.style.cursor = 'default'
  }

  function onCanvasClick(e) {
    if (!points.length) return
    const { mx, my } = getLogicalXY(e)

    // Context menu hit test
    if (ctxMenu) {
      const item = menuItemAt(mx, my)
      if (item) {
        handleMenuAction(item.action, ctxMenu.p)
      }
      ctxMenu = null
      render()
      return
    }

    // Was this a lasso drag? If so, don't fire dot click
    if (lassoDragged) return

    const hit = hitTest(mx, my)
    if (hit) {
      // Show context menu instead of immediately loading
      ctxMenu = { cx: hit.cx, cy: hit.cy, p: hit.p }
      render()
    } else {
      // Click on empty space: clear lasso selection
      if (lassoSelected.size > 0) {
        lassoSelected = new Set()
        render()
      }
    }
  }

  function onKeyDown(e) {
    if (e.key === 'Escape') {
      let changed = false
      if (ctxMenu)             { ctxMenu = null; changed = true }
      if (lassoSelected.size)  { lassoSelected = new Set(); changed = true }
      if (lassoActive)         { lassoActive = false; lassoPath = []; changed = true }
      if (changed) render()
    }
  }

  /* ── Context menu actions ────────────────────────────────────── */
  async function handleMenuAction(action, p) {
    if (action === 'play') {
      await loadAndSelectSound(p)
    } else if (action === 'similar') {
      document.dispatchEvent(new CustomEvent('radar-find-similar', {
        detail: { sound_id: p.sound_id, filename: p.filename },
      }))
    }
  }

  /* ── Similarity strip ───────────────────────────────────────── */
  let similarityN = 8   // rows each side of center
  let similarityAnchor = null  // the currently-centered point

  function buildSimilarityStrip(anchorPoint) {
    similarityAnchor = anchorPoint
    const container = document.getElementById('radar-similarity-strip')
    if (!container) return

    // Compute Euclidean distance from anchor to all other points
    const neighbors = points
      .filter(p => p.sound_id !== anchorPoint.sound_id)
      .map(p => ({
        ...p,
        dist: Math.hypot(p.x - anchorPoint.x, p.y - anchorPoint.y)
      }))
      .sort((a, b) => a.dist - b.dist)

    // N most similar ABOVE center (displayed nearest-to-center last, so reversed in DOM)
    const above = neighbors.slice(0, similarityN).reverse()
    // next N below center
    const below = neighbors.slice(similarityN, similarityN * 2)

    const rowHtml = (p, cls) => {
      const dur = p.duration != null ? fmtDur(p.duration) : '—'
      const distPct = (p.dist * 100).toFixed(1)
      const col = colorFor(p.collection)
      return `<div class="sim-row ${cls}" data-id="${p.sound_id}" title="${p.filename}">
        <span class="sim-dot" style="background:${col}"></span>
        <span class="sim-name">${p.filename.replace(/\.[^.]+$/, '')}</span>
        <span class="sim-meta">${p.collection || ''}</span>
        <span class="sim-dur">${dur}</span>
        <span class="sim-dist">${distPct}%</span>
      </div>`
    }

    const anchorHtml = `<div class="sim-row sim-anchor" data-id="${anchorPoint.sound_id}">
      <span class="sim-dot sim-dot-anchor" style="background:${colorFor(anchorPoint.collection)}"></span>
      <span class="sim-name">${anchorPoint.filename.replace(/\.[^.]+$/, '')}</span>
      <span class="sim-meta">${anchorPoint.collection || ''}</span>
      <span class="sim-dur">${anchorPoint.duration != null ? fmtDur(anchorPoint.duration) : '—'}</span>
      <span class="sim-dist">—</span>
    </div>`

    container.innerHTML =
      above.map(p => rowHtml(p, 'sim-above')).join('') +
      anchorHtml +
      below.map(p => rowHtml(p, 'sim-below')).join('')

    // Click any row to select that sound
    container.querySelectorAll('.sim-row[data-id]').forEach(row => {
      row.addEventListener('click', () => {
        const id = parseInt(row.dataset.id, 10)
        const p = points.find(pt => pt.sound_id === id)
        if (p) {
          selectedSoundId = id
          render()
          loadAndSelectSound(p)
        }
      })
    })

    // Scroll anchor into view
    const anchorEl = container.querySelector('.sim-anchor')
    if (anchorEl) anchorEl.scrollIntoView({ block: 'center', behavior: 'smooth' })

    document.getElementById('radar-sim-panel')?.style.setProperty('display', 'flex')
  }

  function updateSimilaritySlider(n) {
    similarityN = Math.max(2, Math.min(50, parseInt(n, 10)))
    const label = document.getElementById('sim-n-label')
    if (label) label.textContent = similarityN
    if (similarityAnchor) buildSimilarityStrip(similarityAnchor)
  }

  window.updateSimilaritySlider = updateSimilaritySlider

  /* ── Sidebar: collection filter ─────────────────────────────── */
  function buildSidebar() {
    const list = document.getElementById('radar-sb-list')
    if (!list) return

    // Count points per collection
    const counts = {}
    points.forEach(p => {
      const key = p.collection || 'unknown'
      counts[key] = (counts[key] || 0) + 1
    })

    const collections = Object.keys(counts).sort()
    list.innerHTML = collections.map(col => {
      const color = colorFor(col)
      return `<div class="radar-sb-item" data-col="${col}" onclick="radarSidebarToggle('${col.replace(/'/g,'\\\'')}')" title="${col}">
        <span class="radar-sb-dot" style="background:${color}"></span>
        <span style="overflow:hidden;text-overflow:ellipsis;">${col}</span>
        <span class="radar-sb-count">${counts[col]}</span>
      </div>`
    }).join('')
  }

  function updateSidebarActive() {
    const list = document.getElementById('radar-sb-list')
    if (!list) return
    list.querySelectorAll('.radar-sb-item').forEach(el => {
      const col = el.dataset.col
      el.classList.toggle('active', activeFilter.has(col))
    })
    const allBtn = document.getElementById('radar-sb-all')
    if (allBtn) allBtn.classList.toggle('active', activeFilter.size === 0)
  }

  window.radarSidebarToggle = function(col) {
    if (activeFilter.has(col)) {
      activeFilter.delete(col)
    } else {
      activeFilter.add(col)
    }
    updateSidebarActive()
    if (canvas && points.length) render()
  }

  window.radarSidebarSelectAll = function() {
    activeFilter.clear()
    updateSidebarActive()
    if (canvas && points.length) render()
  }

  /* ── Resize handle (canvas ↕ sim panel) ─────────────────────── */
  function attachResizeHandle() {
    const handle   = document.getElementById('radar-resize-handle')
    const simPanel = document.getElementById('radar-sim-panel')
    const mainEl   = document.getElementById('radar-main')
    if (!handle || !simPanel || !mainEl) return
    if (handle._resizeAttached) return
    handle._resizeAttached = true

    let dragging = false, startY = 0, startH = 0

    handle.addEventListener('mousedown', e => {
      e.preventDefault()
      dragging = true
      startY   = e.clientY
      startH   = simPanel.offsetHeight
      handle.classList.add('dragging')
      document.body.style.cursor = 'ns-resize'
      document.body.style.userSelect = 'none'
    })

    document.addEventListener('mousemove', e => {
      if (!dragging) return
      const delta  = startY - e.clientY   // positive = drag up = sim panel grows
      const newH   = Math.max(60, Math.min(window.innerHeight * 0.7, startH + delta))
      simPanel.style.height    = newH + 'px'
      simPanel.style.maxHeight = 'none'
      // Trigger canvas resize so it fills remaining space
      if (canvas) { sizeCanvas(); render() }
    })

    document.addEventListener('mouseup', () => {
      if (!dragging) return
      dragging = false
      handle.classList.remove('dragging')
      document.body.style.cursor = ''
      document.body.style.userSelect = ''
    })
  }

  async function loadAndSelectSound(p) {
    try {
      const r = await fetch(`/api/sounds/${p.sound_id}`)
      if (!r.ok) {
        console.warn('radar: sound fetch failed', r.status)
        document.dispatchEvent(new CustomEvent('show-toast', {
          detail: { msg: 'Sound not found', type: 'error' },
        }))
        return
      }
      const data  = await r.json()
      const sound = (data && data.path) ? data : (data.sound ?? data)
      if (!sound || !sound.path) {
        console.warn('radar: unexpected response shape', data)
        return
      }
      document.dispatchEvent(new CustomEvent('sound-selected', { detail: sound }))
      // Update similarity strip centered on this point
      buildSimilarityStrip(p)
      const filterValue = p.folder_path || null
      if (filterValue) {
        document.dispatchEvent(new CustomEvent('radar-select-folder', {
          detail: filterValue,
        }))
      }
    } catch (err) {
      console.warn('radar: loadAndSelectSound error', err)
    }
  }
})()
