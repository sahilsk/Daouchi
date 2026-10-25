// Desktop-shell integration (Tauri). No-op in a plain browser: everything is gated on window.__TAURI__.
// Makes the exported-clip filename in #export-result draggable into Finder or any editor.
;(function () {
  'use strict'
  var T = window.__TAURI__
  if (!T || !T.drag || typeof T.drag.startDrag !== 'function') return

  var ICON = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAYAAACqaXHeAAABGElEQVR42u3aOxLCIBCA4T2GhaVn8ZwezBvY2Fjr6IxNBpDNPkjkLyhkEtj9gqOBlcPx9Jy5CQAAAAAAAAAAAAAAAMS28+Wqan8BoE16BIZsOfEMCNlL8lEIkp141jipAFHBZiBIZPIZq2soQOZ3NWquEIBf993uj0+rffaeLwTAmnwJYASCjEq+BpCN4AbQc58XgCUGM4BlYk8ALwQzgCZgb4D3tZsA6A3aE+B7bSpAacm1gl72rwFojV0C0CKIx9MvBVlLZg1Aa3zrKgAgAqCnXwvQ6gcAAAAAAICfQQDyAab7Kzz9yxCvw2yIsCXGpijb4hyMcDTG4SjH4xRIUCJDkRRlchRKUioLAAAAAAAAAAAAAMCe2ws1SIou30rvcQAAAABJRU5ErkJggg=='

  function arm (el) {
    if (el.dataset.dragArmed) return
    el.dataset.dragArmed = '1'
    el.style.cursor = 'grab'
    el.style.userSelect = 'none'
    el.title = (el.title || '') + '\nDrag into Finder or your editor'
    var path = null
    el.addEventListener('mousedown', function (e) {
      if (e.button !== 0) return
      path = el.dataset.path || (el.title || '').split('\n')[0]
      var x = e.clientX, y = e.clientY
      function move (ev) {
        if (Math.abs(ev.clientX - x) + Math.abs(ev.clientY - y) < 5) return
        cleanup()
        if (path) {
          // Absolute path of the exported WAV; the OS takes over the drag session.
          T.drag.startDrag({ item: [path], icon: ICON }).catch(function (err) { console.warn('drag-out failed', err) })
        }
      }
      function cleanup () {
        window.removeEventListener('mousemove', move, true)
        window.removeEventListener('mouseup', cleanup, true)
      }
      window.addEventListener('mousemove', move, true)
      window.addEventListener('mouseup', cleanup, true)
    })
  }

  function scan () {
    var box = document.getElementById('export-result')
    if (!box) return
    box.querySelectorAll('.export-result-path').forEach(function (el) {
      // showExportResult() puts the absolute path in the title attribute; remember it before we extend the title.
      if (!el.dataset.path) el.dataset.path = el.title
      arm(el)
    })
  }

  function init () {
    var box = document.getElementById('export-result')
    if (!box) return
    new MutationObserver(scan).observe(box, { childList: true })
    scan()
  }
  if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', init)
  else init()
})()
