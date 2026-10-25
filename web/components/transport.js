// Desktop transport shim. The Tauri shell serves this UI through the daouchi:// custom URI scheme, whose
// responses are not streamed, so Server-Sent Events cannot work there. When running inside the desktop shell
// this replaces window.EventSource with a polling implementation of the two SSE endpoints the UI uses;
// in a plain browser (or anywhere else) the native EventSource is left untouched. Must load before models.js/app.js.
;(function () {
  'use strict'
  var desktop = !!window.__TAURI__ || !!window.__TAURI_INTERNALS__ ||
    location.protocol === 'daouchi:' || location.hostname === 'daouchi.localhost'
  if (!desktop) return

  // SSE url -> polling source. `snapshot` is the endpoint returning the same payload the SSE data carries.
  var SOURCES = {
    '/api/models/events': { url: '/api/models', fast: 1000, idle: 4000,
      busy: function (d) { return Array.isArray(d) && d.some(function (m) { return m && m.downloading }) } },
    '/api/index/progress': { url: '/api/index/status', fast: 500, idle: 500,
      busy: function () { return true },
      done: function (d) { return d && (d.status === 'done' || d.status === 'cancelled' || String(d.status || '').indexOf('error') === 0) } }
  }

  var Native = window.EventSource

  function PollingEventSource (url) {
    var path = String(url).split('?')[0]
    var cfg = SOURCES[path]
    if (!cfg) { if (Native) return new Native(url); throw new Error('EventSource unsupported: ' + url) }
    var self = this
    this.url = url
    this.readyState = 0 // CONNECTING
    this.onopen = this.onmessage = this.onerror = null
    this._listeners = {}
    this._last = null
    this._timer = null
    this._closed = false

    function emit (type, ev) {
      var h = self['on' + type]
      if (typeof h === 'function') h.call(self, ev)
      ;(self._listeners[type] || []).slice().forEach(function (f) { f.call(self, ev) })
    }

    function tick () {
      if (self._closed) return
      fetch(cfg.url, { cache: 'no-store' }).then(function (r) {
        if (!r.ok) throw new Error('HTTP ' + r.status)
        return r.text()
      }).then(function (text) {
        if (self._closed) return
        if (self.readyState === 0) { self.readyState = 1; emit('open', { type: 'open' }) }
        var data = JSON.parse(text)
        // like SSE: emit on connect and on every change
        if (text !== self._last) {
          self._last = text
          emit('message', { type: 'message', data: text })
        }
        // A finished index run ends the stream; the SSE stream closes server-side and fires onerror.
        if (self._closed) return // the message handler closed us
        if (cfg.done && cfg.done(data)) { self.close(); emit('error', { type: 'error' }); return }
        self._timer = setTimeout(tick, cfg.busy(data) ? cfg.fast : cfg.idle)
      }).catch(function () {
        if (self._closed) return
        self.readyState = 2
        self._closed = true
        emit('error', { type: 'error' })
      })
    }

    this.close = function () {
      self._closed = true
      self.readyState = 2
      clearTimeout(self._timer)
    }
    this.addEventListener = function (t, f) { (self._listeners[t] = self._listeners[t] || []).push(f) }
    this.removeEventListener = function (t, f) {
      self._listeners[t] = (self._listeners[t] || []).filter(function (x) { return x !== f })
    }
    setTimeout(tick, 0)
  }
  PollingEventSource.CONNECTING = 0
  PollingEventSource.OPEN = 1
  PollingEventSource.CLOSED = 2

  window.EventSource = PollingEventSource
})()
