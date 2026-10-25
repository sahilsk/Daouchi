;(function () {
  'use strict'

  window.U = {
    esc(s) {
      return String(s == null ? '' : s)
        .replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;')
        .replace(/"/g, '&quot;')
    },

    duration(secs) {
      if (secs == null) return '—'
      const s = Math.floor(secs)
      const m = Math.floor(s / 60)
      const h = Math.floor(m / 60)
      if (h > 0) return `${h}:${String(m % 60).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`
      return `${m}:${String(s % 60).padStart(2, '0')}`
    },

    size(bytes) {
      if (!bytes) return ''
      if (bytes >= 1048576) return (bytes / 1048576).toFixed(1) + ' MB'
      return (bytes / 1024).toFixed(0) + ' KB'
    },

    num(n) {
      return Number(n || 0).toLocaleString('en-US')
    },

    // Elapsed / remaining time in plain words: "38 s", "2 min 5 s", "1 h 4 min".
    spoken(ms) {
      const s = Math.max(0, Math.round((ms || 0) / 1000))
      if (s < 60) return `${s} s`
      const m = Math.floor(s / 60)
      if (m < 60) return s % 60 ? `${m} min ${s % 60} s` : `${m} min`
      return `${Math.floor(m / 60)} h ${m % 60} min`
    },

    // "about 2 min left"; '' when unknown.
    etaText(ms) {
      if (ms == null) return ''
      const s = ms / 1000
      if (s < 10) return 'a few seconds left'
      if (s < 60) return 'less than a minute left'
      const m = Math.round(s / 60)
      if (m < 60) return `about ${m} min left`
      return `about ${Math.floor(m / 60)} h ${m % 60} min left`
    },

    badgeClass(ext) {
      switch ((ext || '').toLowerCase()) {
        case 'wav':  return 'badge badge-wav'
        case 'mp3':  return 'badge badge-mp3'
        case 'aiff': return 'badge badge-aiff'
        case 'flac': return 'badge badge-flac'
        case 'ogg':  return 'badge badge-ogg'
        default:     return 'badge badge-other'
      }
    },

    durClass(secs) {
      if (!secs || secs < 2)  return 'dur-short'
      if (secs < 10)          return 'dur-medium'
      return 'dur-long'
    },
  }
})()
