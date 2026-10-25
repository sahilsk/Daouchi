function debounce(fn, ms) {
  let t
  return function (...args) {
    clearTimeout(t)
    t = setTimeout(() => fn.apply(this, args), ms)
  }
}

function daouchi() {
  return {
    /* ── Tabs ─────────────────────────────────────────────────── */
    activeTab: 'library',  // 'library' | 'dashboard' | 'radar'
    playingId: null,       // id of the sound currently playing (drives artwork button icon)

    /* ── Search state ─────────────────────────────────────────── */
    query: '',
    semanticMode: false,
    results: [],
    total: 0,
    loading: false,
    page: 0,
    sortOrder: 'date_desc',

    /* ── Filters ──────────────────────────────────────────────── */
    filterSource: '',
    filterExt: '',
    filterDurMin: null,
    filterDurMax: null,
    filterFavorite: false,
    filterLabel: 0,
    filterMinRating: 0,
    filterTags: [],
    filterBundle: null,
    activeFolderPath: null,

    /* ── Selection ────────────────────────────────────────────── */
    selectedSound: null,
    selectedIndex: -1,

    /* ── Virtual scroll ───────────────────────────────────────── */
    visibleStart: 0,
    ROW_HEIGHT: 62,
    VISIBLE_ROWS: 40,

    /* ── Multi-select & bulk tags ─────────────────────────────── */
    selectedMap: {},           // { [soundId]: true }
    bulkTagModalOpen: false,
    bulkTagInput: '',

    /* ── Folder tree ──────────────────────────────────────────── */
    folderTree: [],
    folderExpanded: {},

    /* ── Tags ─────────────────────────────────────────────────── */
    allTags: [],
    tagInput: '',

    /* ── Bundles ──────────────────────────────────────────────── */
    bundles: [],
    newBundleName: '',
    showCreateBundle: false,

    /* ── Projects ────────────────────────────────────────────── */
    projects: [],
    activeProjectId: null,
    activeProject: null,
    projectSounds: [],
    projectSoundsLoading: false,

    /* ── Indexing ─────────────────────────────────────────────── */
    indexing: false,
    indexProgress: { done: 0, total: 0, status: '', phase: '' },
    indexSummary: null,          // final snapshot of the last run (done|cancelled|error), until dismissed
    indexCancelConfirm: false,   // inline "stop indexing?" confirm is open
    indexCancelling: false,      // cancel requested, waiting for the backend's 'cancelled' snapshot
    indexErrorsOpen: false,
    _indexEs: null,
    showIndexModal: false,
    indexFolders: [{ path: '', source: 'sfx', _uid: 1 }],

    /* ── Radar ────────────────────────────────────────────────── */
    radarStatus: 'none',
    radarProgress: { done: 0, total: 0 },
    radarError: '',          // set when /api/radar/status reports state:"error"
    radarIncomplete: false,  // embeddings from the current model don't cover the library yet
    radarGenerating: false,
    showRadar: false,
    radarPollInterval: null,
    waitForReadyTimeout: null,

    /* ── Autoplay ─────────────────────────────────────────────── */
    autoplay: localStorage.getItem('daouchi_autoplay') === 'true',

    /* ── Theme ────────────────────────────────────────────────── */
    themeLight: localStorage.getItem('daouchi_theme') === 'light',

    /* ── Audio FX ────────────────────────────────────────────── */
    activeFX: 'none',

    /* ── Toast ────────────────────────────────────────────────── */
    toast: null,

    /* ── Command palette ──────────────────────────────────────── */
    showCmdPalette: false,
    cmdQuery: '',
    cmdActiveIdx: 0,

    /* ── Shortcut overlay ─────────────────────────────────────── */
    showShortcuts: false,

    /* ── JKL varispeed / IO markers ──────────────────────────── */
    _jklSpeed: 1,
    _jklDir: 'forward',

    /* ── Dashboard ────────────────────────────────────────────── */
    dashboardStats: { total: 0, totalDuration: 0, sfxCount: 0, musicCount: 0, withEmbeddings: 0, withStems: 0 },
    collectionBreakdown: [],
    recentSounds: [],
    needsAttention: [],

    /* ── Lifecycle ────────────────────────────────────────────── */
    async init() {
      // Apply persisted theme before first render
      if (this.themeLight) document.documentElement.setAttribute('data-theme', 'light')

      // Parse URL hash for tab + optional project ID
      const hash = location.hash.slice(1)  // e.g. "projects/3" or "dashboard"
      if (hash) {
        const [tab, projectId] = hash.split('/')
        const validTabs = ['library', 'dashboard', 'projects']
        if (validTabs.includes(tab)) {
          this.activeTab = tab
          if (tab === 'projects' && projectId) this._pendingProjectId = parseInt(projectId, 10)
        }
      }

      await Promise.all([this.loadFolders(), this.loadRoots(), this.loadTags(), this.loadBundles(), this.loadProjects()])
      await this.search()

      if (typeof window.projectsInit === 'function') window.projectsInit()

      // If launched with a project hash, open it after DOM settles
      if (this._pendingProjectId) {
        this.$nextTick(() => this.openProject(this._pendingProjectId))
        this._pendingProjectId = null
      }
      if (this.activeTab === 'dashboard') {
        this.$nextTick(() => { if (typeof window.dashboardInit === 'function') window.dashboardInit() })
      }

      this.$watch('query', () => this.debouncedSearch())
      ;['filterSource', 'filterExt', 'filterDurMin', 'filterDurMax',
        'filterFavorite', 'filterLabel', 'filterMinRating', 'filterBundle', 'activeFolderPath', 'filterTags']
        .forEach(f => this.$watch(f, () => this.search()))

      // Watch activeTab and load dashboard/radar when switching to them
      this.$watch('activeTab', tab => {
        if (tab === 'dashboard') this.loadDashboard()
        if (tab === 'radar') {
          // Immediate poll so any recent server state is reflected without delay.
          // pollRadarStatus will dispatch show-radar when it confirms status === 'ready'.
          clearTimeout(this.radarPollInterval)
          this.pollRadarStatus()
        }
      })

      // Close project dropdowns when clicking outside
      document.addEventListener('click', () => {
        document.querySelectorAll('.project-dropdown.open').forEach(d => d.classList.remove('open'))
      })

      // Navigate results with arrow keys from player.js
      document.addEventListener('navigate-results', (e) => {
        this.navigateResult(e.detail)
      })

      // Sync label changes made from the player panel back to the results list
      document.addEventListener('sound-label-changed', (e) => {
        const { id, label } = e.detail
        const s = this.results.find(r => r.id === id)
        if (s) s.label = label
        if (this.selectedSound && this.selectedSound.id === id) this.selectedSound.label = label
      })

      document.addEventListener('radar-select-folder', (e) => {
        this.activeFolderPath = e.detail
        this.loading = true
        this.search()
      })

      // Keyboard shortcuts — single listener covers all cases
      document.addEventListener('keydown', (e) => {
        // Cmd/Ctrl+K — command palette
        if ((e.metaKey || e.ctrlKey) && e.key === 'k') {
          e.preventDefault()
          if (this.showCmdPalette) this.closeCmdPalette()
          else this.openCmdPalette()
          return
        }

        // Alt+S — toggle semantic mode (before input guard so it works everywhere)
        if (e.altKey && e.key === 's') {
          e.preventDefault()
          this.semanticMode = !this.semanticMode
          this.search()
          return
        }

        // ? — toggle shortcuts overlay (when no input focused)
        if (e.key === '?' && !this._isInputFocused()) {
          e.preventDefault()
          this.showShortcuts = !this.showShortcuts
          return
        }

        // Escape — close overlays
        if (e.key === 'Escape') {
          if (this.showCmdPalette) { this.closeCmdPalette(); return }
          if (this.showShortcuts) { this.showShortcuts = false; return }
        }

        // Cmd/Ctrl shortcuts
        if (e.metaKey || e.ctrlKey) {
          if (e.key === 'l' || e.key === 'L') { e.preventDefault(); this.switchTab('library'); return }
          if (e.key === 'd' || e.key === 'D') { e.preventDefault(); this.switchTab('dashboard'); return }
          if (e.key === 'r' || e.key === 'R') { e.preventDefault(); this.switchTab('radar'); return }
          if (e.key === 'p' || e.key === 'P') { e.preventDefault(); this.switchTab('projects'); return }
          if (e.key === 'f' || e.key === 'F') {
            e.preventDefault()
            const si = document.getElementById('search-input')
            if (si) si.focus()
            return
          }
          if (e.key === 'e' || e.key === 'E') {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('export-clip'))
            return
          }
        }

        // JKL varispeed, IO markers, ArrowUp/Down, [ ] — only when no input focused
        if (!this._isInputFocused()) {

          // ── ArrowUp / ArrowDown — row navigation ────────────────
          if (e.key === 'ArrowUp') {
            e.preventDefault()
            if (this.results.length) {
              const next = Math.max(0, this.selectedIndex - 1)
              this.selectSound(this.results[next], next)
              const rows = document.querySelectorAll('.result-row')
              if (rows[next]) rows[next].scrollIntoView({ block: 'nearest' })
              if (this.autoplay) document.dispatchEvent(new CustomEvent('autoplay-selected'))
            }
            return
          }
          if (e.key === 'ArrowDown') {
            e.preventDefault()
            if (this.results.length) {
              const next = Math.min(this.results.length - 1, this.selectedIndex + 1)
              this.selectSound(this.results[next], next)
              const rows = document.querySelectorAll('.result-row')
              if (rows[next]) rows[next].scrollIntoView({ block: 'nearest' })
              if (this.autoplay) document.dispatchEvent(new CustomEvent('autoplay-selected'))
            }
            return
          }

          // ── K — pause ───────────────────────────────────────────
          if (e.key === 'k') {
            e.preventDefault()
            this._jklSpeed = 1
            this._jklDir   = 'forward'
            document.dispatchEvent(new CustomEvent('jkl-pause'))
            return
          }

          // ── L — play forward (1x → 2x → 4x) ────────────────────
          if (e.key === 'l') {
            e.preventDefault()
            if (this._jklDir !== 'forward') {
              // switching direction: reset speed
              this._jklSpeed = 1
              this._jklDir   = 'forward'
            } else {
              // already forward: cycle speed
              if      (this._jklSpeed === 1) this._jklSpeed = 2
              else if (this._jklSpeed === 2) this._jklSpeed = 4
              else                           this._jklSpeed = 1
            }
            document.dispatchEvent(new CustomEvent('jkl-play', { detail: { rate: this._jklSpeed, dir: 'forward' } }))
            return
          }

          // ── J — play backward (1x → 2x → 4x) ───────────────────
          // NOTE: WaveSurfer v7 does not natively support negative playback rates.
          // The 'jkl-play' event with dir:'backward' is dispatched so player.js can
          // handle it (e.g. seek near end and play at positive rate, or use AudioBuffer reversal).
          if (e.key === 'j') {
            e.preventDefault()
            if (this._jklDir !== 'backward') {
              this._jklSpeed = 1
              this._jklDir   = 'backward'
            } else {
              if      (this._jklSpeed === 1) this._jklSpeed = 2
              else if (this._jklSpeed === 2) this._jklSpeed = 4
              else                           this._jklSpeed = 1
            }
            document.dispatchEvent(new CustomEvent('jkl-play', { detail: { rate: this._jklSpeed, dir: 'backward' } }))
            return
          }

          // ── I / O — in/out markers ───────────────────────────────
          if (e.key === 'i' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('set-inpoint'))
            return
          }
          if (e.key === 'o' && !e.shiftKey && !e.metaKey && !e.ctrlKey && !e.altKey) {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('set-outpoint'))
            return
          }
          if (e.key === 'I' && e.shiftKey) {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('seek-to-inpoint'))
            return
          }
          if (e.key === 'O' && e.shiftKey) {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('seek-to-outpoint'))
            return
          }

          // ── Option+X — clear in/out markers ─────────────────────
          if (e.altKey && e.key === 'x') {
            e.preventDefault()
            document.dispatchEvent(new CustomEvent('clear-inout'))
            return
          }

          // ── [ and ] — navigate results ───────────────────────────
          if (e.key === '[') { e.preventDefault(); this.navigateResult(-1); return }
          if (e.key === ']') { e.preventDefault(); this.navigateResult(1); return }
        }
      })

      // Lasso selection from radar
      document.addEventListener('radar-lasso-select', (e) => {
        const soundIds = e.detail  // array of sound_ids
        if (!soundIds || !soundIds.length) return
        // Filter results to only the lassoed sounds
        this.results = this.results.filter(r => soundIds.includes(r.id))
        this.total = this.results.length
        this.showToast(this.results.length + ' sounds selected from radar', 'info')
      })

      // Find similar by sound_id
      document.addEventListener('find-similar-by-id', async (e) => {
        const { sound_id } = e.detail
        try {
          this.loading = true
          const r = await fetch('/api/sounds/similar/' + sound_id + '?limit=30')
          if (r.ok) {
            const data = await r.json()
            this.results = data.results || []
            this.total = this.results.length
            this.showToast('Found ' + this.results.length + ' similar sounds', 'info')
          } else {
            this.showToast('Find similar failed: ' + r.status, 'error')
          }
        } catch (_) {
          this.showToast('Find similar failed', 'error')
        } finally {
          this.loading = false
        }
      })

      // Poll radar status
      this.pollRadarStatus()

      // Load dashboard if starting on that tab
      if (this.activeTab === 'dashboard') this.loadDashboard()

      // Dashboard component events
      document.addEventListener('dashboard-filter-tag', (e) => {
        this.filterTags = [e.detail]
        this.loading = true
        this.switchTab('library')
        this.search()
      })
      document.addEventListener('dashboard-filter-collection', (e) => {
        this.activeFolderPath = e.detail
        this.loading = true
        this.switchTab('library')
        this.search()
      })
      document.addEventListener('dashboard-search-results', (e) => {
        this.loading = true
        this.switchTab('library')
        this.$nextTick(() => {
          this.results = e.detail
          this.total = e.detail.length
          this.loading = false
        })
      })
      document.addEventListener('select-sound-by-id', (e) => {
        const { id } = e.detail
        const found = this.results.find(r => r.id === id)
        if (found) {
          this.selectSound(found, this.results.indexOf(found))
        } else {
          // Fetch from API and select
          fetch(`/api/sounds/${id}`).then(r => r.json()).then(s => {
            this.results = [s, ...this.results.filter(r => r.id !== id)]
            this.selectSound(s, 0)
            this.switchTab('library')
          }).catch(() => {})
        }
      })

      // Cleanup: stop recurring timeouts when Alpine destroys this component
      this.$cleanup = () => {
        clearTimeout(this.radarPollInterval)
        clearTimeout(this.waitForReadyTimeout)
      }
    },

    /* ── Tab helpers ──────────────────────────────────────────── */
    switchTab(tab, projectId) {
      this.activeTab = tab
      if (tab === 'library' || tab === 'radar') {
        // Library is default; radar is transient — neither gets a persistent hash
        history.replaceState(null, '', location.pathname)
      } else if (projectId) {
        history.replaceState(null, '', `#${tab}/${projectId}`)
      } else {
        history.replaceState(null, '', `#${tab}`)
      }
      if (tab === 'dashboard') {
        this.$nextTick(() => {
          if (typeof window.dashboardInit === 'function') window.dashboardInit()
        })
      }
      if (tab === 'radar') {
        // Always do an immediate status poll when switching to radar so we
        // never miss a server restart that changed status from 'computing' to 'ready'.
        clearTimeout(this.radarPollInterval)
        this.pollRadarStatus()
      }
      if (tab === 'projects' && projectId) {
        this.$nextTick(() => this.openProject(projectId))
      }
    },

    /* ── Input focus detection ────────────────────────────────── */
    _isInputFocused() {
      const el = document.activeElement
      if (!el) return false
      const tag = el.tagName
      return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || el.isContentEditable
    },

    debouncedSearch: debounce(function () { this.search() }, 180),

    /* ── Virtual scroll helpers ───────────────────────────────── */
    visibleResults() {
      return this.results.slice(this.visibleStart, this.visibleStart + this.VISIBLE_ROWS)
    },

    handleResultsScroll(e) {
      const scrollTop = e.target.scrollTop
      this.visibleStart = Math.max(0, Math.floor(scrollTop / this.ROW_HEIGHT) - 5)
    },

    /* ── Search ───────────────────────────────────────────────── */
    async search() {
      this.loading = true
      this.page = 0
      this.visibleStart = 0
      try {
        // Semantic search needs CLAP; until it's downloaded fall back to keyword
        // search (the inline Download prompt is shown above the results).
        if (this.semanticMode && this.query.trim() && !this.$store.models.isMissing('clap')) {
          await this._semanticSearch()
        } else {
          await this._keywordSearch()
        }
      } catch (err) {
        console.error('search error', err)
        this.results = []
        this.total   = 0
        this.showToast('Search failed', 'error')
      } finally {
        this.loading = false
      }
    },

    async _keywordSearch() {
      const params = new URLSearchParams()
      if (this.query)                                                         params.set('q',         this.query)
      if (this.filterSource)                                                  params.set('source',    this.filterSource)
      if (this.filterExt)                                                     params.set('ext',       this.filterExt)
      if (this.filterDurMin !== null && this.filterDurMin !== '' && !isNaN(this.filterDurMin))  params.set('dur_min',   this.filterDurMin)
      if (this.filterDurMax !== null && this.filterDurMax !== '' && !isNaN(this.filterDurMax))  params.set('dur_max',   this.filterDurMax)
      if (this.filterFavorite)        params.set('favorite',  '1')
      if (this.filterLabel)           params.set('label',     this.filterLabel)
      if (this.filterMinRating)       params.set('min_rating',this.filterMinRating)
      if (this.filterTags.length)     this.filterTags.forEach(t => params.append('tag', t))
      if (this.filterBundle)          params.set('bundle_id', this.filterBundle)
      if (this.activeFolderPath)      params.set('folder',    this.activeFolderPath)
      params.set('sort',   this.sortOrder)
      params.set('limit',  '50')
      params.set('offset', '0')

      const r = await fetch('/api/sounds/search?' + params)
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      this.results = (data.results || []).map(r => ({ ...r, _semanticScore: null }))
      this.total   = data.total   || 0
    },

    async _semanticSearch() {
      const r = await fetch('/api/sounds/semantic_search', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ query: this.query, limit: 50 }),
      })
      if (r.status === 409) { await this._keywordSearch(); return }
      if (!r.ok) throw new Error(r.statusText)
      const data = await r.json()
      this.results = (data.results || []).map(r => ({ ...r, _semanticScore: r.score }))
      this.total   = data.total || 0
    },

    /* ── Load helpers ─────────────────────────────────────────── */
    async loadFolders() {
      try {
        const r    = await fetch('/api/folders')
        const data = await r.json()
        this.folderTree = data.tree || []
      } catch (e) { console.warn('loadFolders', e); this.showToast('Could not load folder tree', 'error') }
    },

    async loadTags() {
      try {
        const r    = await fetch('/api/tags')
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        this.allTags = data.tags || []
      } catch (e) { console.warn('loadTags', e); this.showToast('Could not load tags', 'error') }
    },

    async loadBundles() {
      try {
        const r    = await fetch('/api/bundles')
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        this.bundles = data.bundles || []
      } catch (e) { console.warn('loadBundles', e); this.showToast('Could not load bundles', 'error') }
    },

    /* ── Dashboard ────────────────────────────────────────────── */
    async loadDashboard() {
      this.loading = true
      try {
        // NOTE: limit=1000 means derived per-row counts (sfxCount, withEmbeddings, etc.)
        // are approximate for libraries > 1000 sounds. dashboardStats.total always uses
        // the server-reported total which is authoritative.
        const r = await fetch('/api/sounds/search?limit=1000&offset=0')
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        const sounds = data.results || []

        // Use server total for the count card — it's always correct regardless of limit
        this.dashboardStats.total           = data.total || sounds.length
        this.dashboardStats.totalDuration   = sounds.reduce((s, x) => s + (x.duration || 0), 0)
        this.dashboardStats.sfxCount        = sounds.filter(s => s.source === 'sfx').length
        this.dashboardStats.musicCount      = sounds.filter(s => s.source === 'music').length
        this.dashboardStats.withEmbeddings  = sounds.filter(s => s.has_embedding).length
        this.dashboardStats.withStems       = sounds.filter(s => s.has_stems).length

        // Collection breakdown
        const byCollection = {}
        sounds.forEach(s => {
          const col = s.collection || 'Unknown'
          if (!byCollection[col]) byCollection[col] = { name: col, count: 0, totalDuration: 0, source: s.source }
          byCollection[col].count++
          byCollection[col].totalDuration += s.duration || 0
        })
        this.collectionBreakdown = Object.values(byCollection)
          .map(c => ({ ...c, avgDuration: c.totalDuration / c.count }))
          .sort((a, b) => b.count - a.count)
          .slice(0, 20)

        // Recent sounds — sort by indexed_at descending
        this.recentSounds = sounds
          .slice()
          .sort((a, b) => (b.indexed_at || '').localeCompare(a.indexed_at || ''))
          .slice(0, 10)

        // Needs attention — no tags
        this.needsAttention = sounds
          .filter(s => !s.tags || s.tags.length === 0)
          .slice(0, 20)
      } catch (e) {
        console.warn('loadDashboard', e)
      } finally {
        this.loading = false
      }
    },

    /* ── Collection color ─────────────────────────────────────── */
    collectionColor(name) {
      if (!name) return 'hsl(0, 60%, 55%)'
      let hash = 0
      for (let i = 0; i < name.length; i++) hash = name.charCodeAt(i) + ((hash << 5) - hash)
      const hue = Math.abs(hash) % 360
      return `hsl(${hue}, 60%, 55%)`
    },

    durClass(secs)        { return window.U.durClass(secs) },

    /* ── Score color (also exposed on window via player.js) ──────── */
    scoreColor(score) {
      if (!score) return '#666666'
      if (score >= 0.8) return '#3dba6e'
      if (score >= 0.5) return '#c9a227'
      return '#d05454'
    },

    /* ── Selection ────────────────────────────────────────────── */
    selectSound(sound, idx) {
      this.selectedSound = sound
      this.selectedIndex = idx !== undefined ? idx : this.results.indexOf(sound)
      this.$nextTick(() => {
        document.dispatchEvent(new CustomEvent('sound-selected', { detail: sound }))
      })
    },

    // Row play button — drives the main player, not a separate mini-player.
    // If the sound is already loaded: toggle play/pause.
    // If it's a different sound: select it and play once loaded.
    libRowTogglePlay(sound) {
      if (this.selectedSound && this.selectedSound.id === sound.id) {
        if (window.playerPlay) window.playerPlay()
      } else {
        document.dispatchEvent(new CustomEvent('play-after-load'))
        this.selectSound(sound)
      }
    },

    hoverSound(sound) {
      document.dispatchEvent(new CustomEvent('result-row-hovered', { detail: sound }))
    },

    navigateResult(delta) {
      if (!this.results.length) return
      const next = Math.max(0, Math.min(this.results.length - 1, this.selectedIndex + delta))
      if (next !== this.selectedIndex) {
        this.selectSound(this.results[next], next)
        // Scroll the results container to keep the selected row visible
        const container = document.getElementById('results-area')
        if (container) {
          const rowTop = next * this.ROW_HEIGHT
          const rowBot = rowTop + this.ROW_HEIGHT
          if (rowTop < container.scrollTop) {
            container.scrollTop = rowTop
          } else if (rowBot > container.scrollTop + container.clientHeight) {
            container.scrollTop = rowBot - container.clientHeight
          }
        }
      }
    },

    /* ── Folder tree helpers ──────────────────────────────────── */
    toggleFolder(path) {
      this.folderExpanded[path] = !this.folderExpanded[path]
    },

    selectFolder(path) {
      this.loading = true
      this.activeFolderPath = this.activeFolderPath === path ? null : path
      document.dispatchEvent(new CustomEvent('radar-folder-filter', {
        detail: this.activeFolderPath,
      }))
    },

    // The server resolves root + relative path; the client never sends a raw path to open.
    async revealFolder(node) {
      if (!node || node.root_id == null) { this.showToast('This folder is not under a registered library root', 'error'); return }
      await this._reveal({ kind: 'folder', root_id: node.root_id, rel_path: node.rel_path || '' })
    },

    async _reveal(body) {
      try {
        const r = await fetch('/api/reveal', { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify(body) })
        if (!r.ok) this.showToast('Reveal failed: ' + ((await r.text()) || r.statusText), 'error')
      } catch (e) { this.showToast('Reveal failed: ' + e.message, 'error') }
    },

    async loadRoots() {
      try {
        const r = await fetch('/api/library/roots')
        if (r.ok) this.libraryRoots = (await r.json()).roots || []
      } catch (_) {}
    },

    /* ── Remove from library ──────────────────────────────────── */
    removeDlg: { open: false, loading: false, busy: false, title: '', target: null, preview: null, deleteFiles: false, error: '', result: null },
    libraryRoots: [],

    ellipsizeMiddle(p, max = 64) {
      p = p || ''
      if (p.length <= max) return p
      const keep = max - 1, head = Math.ceil(keep * 0.4), tail = keep - head
      return p.slice(0, head) + '…' + p.slice(p.length - tail)
    },

    async _openRemove(title, target) {
      this.removeDlg = { open: true, loading: true, busy: false, title, target, preview: null, deleteFiles: false, error: '', result: null }
      try {
        const r = await fetch('/api/library/remove_preview', { method: 'POST', headers: {'Content-Type':'application/json'}, body: JSON.stringify(target) })
        if (!r.ok) throw new Error((await r.text()) || r.statusText)
        this.removeDlg.preview = await r.json()
      } catch (e) { this.removeDlg.error = e.message }
      this.removeDlg.loading = false
    },

    openRemoveFolder(node) {
      if (!node || node.root_id == null) { this.showToast('This folder is not under a registered library root', 'error'); return }
      this._openRemove('Remove folder from library',
        { kind: 'folder', root_id: node.root_id, rel_path: node.rel_path || '', direct_only: !!node.direct_only })
    },

    openRemoveRoot(root) {
      this._openRemove('Remove indexed folder from library', { kind: 'folder', root_id: root.id, rel_path: '' })
    },

    openRemoveSound(sound) {
      if (!sound) return
      this._openRemove('Remove sound from library', { kind: 'sound', id: sound.id })
    },

    removeDlgNoun(n) { return n === 1 ? '1 file' : `${window.U.num(n)} files` },

    async confirmRemove() {
      const d = this.removeDlg
      if (!d.preview || d.busy) return
      d.busy = true; d.error = ''
      try {
        const r = await fetch('/api/library/remove', {
          method: 'POST', headers: {'Content-Type':'application/json'},
          body: JSON.stringify({ ...d.target, delete_files: !!d.deleteFiles }),
        })
        if (!r.ok) throw new Error((await r.text()) || r.statusText)
        const out = await r.json()
        const failed = out.failed || []
        let msg = `Removed ${window.U.num(out.removed_sounds)} sound${out.removed_sounds === 1 ? '' : 's'}`
        if (d.deleteFiles) msg += `; moved ${window.U.num(out.trashed_files)} file${out.trashed_files === 1 ? '' : 's'} to Trash`
        if (failed.length) {
          d.result = { msg: msg + `. ${failed.length} could not be moved to Trash and stay in the library.`, failed }
        } else {
          d.open = false
        }
        this.showToast(msg, failed.length ? 'error' : 'success')
        if (d.target.kind === 'sound') {
          this.results = this.results.filter(s => s.id !== d.target.id)
          if (this.selectedSound && this.selectedSound.id === d.target.id) this.selectedSound = null
        }
        this.activeFolderPath = null
        await Promise.all([this.loadFolders(), this.loadRoots(), this.search()])
        if (this.loadDashboard) this.loadDashboard()
      } catch (e) { d.error = e.message }
      d.busy = false
    },

    /* ── Label (colored tag icon, cycles 0–5) ────────────────────── */
    async cycleLabel(sound) {
      const next = ((sound.label || 0) + 1) % 6
      const prev = sound.label || 0
      sound.label = next
      if (this.selectedSound && this.selectedSound.id === sound.id) {
        this.selectedSound.label = next
      }
      try {
        const r = await fetch(`/api/sounds/${sound.id}/label`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ label: next }),
        })
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        sound.label = data.label
        if (this.selectedSound && this.selectedSound.id === sound.id) {
          this.selectedSound.label = data.label
        }
      } catch (e) {
        sound.label = prev
        if (this.selectedSound && this.selectedSound.id === sound.id) {
          this.selectedSound.label = prev
        }
        this.showToast('Failed to set label', 'error')
      }
    },

    /* ── Favorite (kept for sidebar filter; no longer shown as a button) ─ */
    async toggleFavorite(sound, event) {
      event.stopPropagation()
      sound.favorite = !sound.favorite // optimistic
      try {
        const r    = await fetch(`/api/sounds/${sound.id}/favorite`, { method: 'POST' })
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        sound.favorite = data.favorite
      } catch (e) {
        sound.favorite = !sound.favorite // roll back
        this.showToast('Failed to toggle favorite', 'error')
      }
    },

    /* ── Star rating ─────────────────────────────────────────── */
    async setRating(sound, rating) {
      const prev = sound.rating || 0
      sound.rating = rating   // optimistic
      // Update selectedSound if it's the same file
      if (this.selectedSound && this.selectedSound.id === sound.id) {
        this.selectedSound.rating = rating
      }
      try {
        const r = await fetch(`/api/sounds/${sound.id}/rating`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ rating }),
        })
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        sound.rating = data.rating
        if (this.selectedSound && this.selectedSound.id === sound.id) {
          this.selectedSound.rating = data.rating
        }
      } catch (e) {
        sound.rating = prev
        if (this.selectedSound && this.selectedSound.id === sound.id) {
          this.selectedSound.rating = prev
        }
        this.showToast('Failed to set rating', 'error')
      }
    },

    /* ── Bulk tags ────────────────────────────────────────────── */
    bulkTagOpen() {
      this.bulkTagInput = ''
      this.bulkTagModalOpen = true
      this.$nextTick(() => {
        const el = document.querySelector('.bulk-tag-modal-box input')
        if (el) el.focus()
      })
    },

    async bulkTagSubmit() {
      const raw = this.bulkTagInput.trim()
      if (!raw) return
      const add = raw.split(',').map(t => t.trim().replace(/^#+/, '')).filter(Boolean)
      const ids = Object.keys(this.selectedMap).map(Number)
      this.bulkTagModalOpen = false
      try {
        const r = await fetch('/api/sounds/bulk_tags', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ sound_ids: ids, add }),
        })
        if (!r.ok) throw new Error(r.statusText)
        const data = await r.json()
        // Update tags in-memory for visible rows
        this.results.forEach(s => {
          if (ids.includes(s.id)) {
            const existing = new Set(s.tags || [])
            add.forEach(t => existing.add(t))
            s.tags = Array.from(existing).sort()
          }
        })
        this.selectedMap = {}
        this.showToast(`Added ${add.length} tag(s) to ${data.updated} file(s)`, 'success')
      } catch(e) {
        this.showToast('Failed to apply tags: ' + e.message, 'error')
      }
    },

    /* ── Projects ────────────────────────────────────────────── */
    async loadProjects() {
      try {
        const r = await fetch('/api/projects')
        if (r.ok) {
          this.projects = await r.json()
          if (typeof window._renderProjectsSidebar === 'function') window._renderProjectsSidebar(this.projects)
        }
      } catch(_) {}
    },

    async newProject() {
      const name = prompt('Project name:')
      if (!name || !name.trim()) return
      const colors = ['#e47a2a','#3dba6e','#4fa8d5','#c9a227','#c25f9c','#7b8dd4']
      const color  = colors[Math.floor(Math.random() * colors.length)]
      try {
        const r = await fetch('/api/projects', {
          method: 'POST',
          headers: {'Content-Type':'application/json'},
          body: JSON.stringify({name: name.trim(), color})
        })
        if (r.ok) await this.loadProjects()
        else this.showToast('Failed to create project', 'error')
      } catch(_) { this.showToast('Network error', 'error') }
    },

    async addSoundToProject(projectId, sound) {
      document.querySelectorAll('.project-dropdown.open').forEach(d => d.classList.remove('open'))
      try {
        const r = await fetch(`/api/projects/${projectId}/sounds`, {
          method: 'POST',
          headers: {'Content-Type':'application/json'},
          body: JSON.stringify({sound_ids: [sound.id]})
        })
        if (r.ok) {
          await this.loadProjects()
          this.showToast('Added to project')
        } else {
          this.showToast('Failed to add to project', 'error')
        }
      } catch(_) { this.showToast('Network error', 'error') }
    },

    async openProject(id) {
      this.activeProjectId = id
      this.activeProject   = this.projects.find(p => p.id === id) || null
      history.replaceState(null, '', `#projects/${id}`)
      this.projectSoundsLoading = true
      this.projectSounds = []
      projWsDestroy()
      try {
        const r = await fetch(`/api/projects/${id}/sounds`)
        if (r.ok) this.projectSounds = await r.json()
      } catch(_) {}
      this.projectSoundsLoading = false
      this.$nextTick(() => projWsInitAll(this.projectSounds))
    },

    async deleteProject(id) {
      if (!confirm('Delete project? Sounds will not be deleted.')) return
      try {
        await fetch(`/api/projects/${id}`, { method: 'DELETE' })
        if (this.activeProjectId === id) {
          this.activeProjectId = null
          this.activeProject   = null
          this.projectSounds   = []
        }
        await this.loadProjects()
      } catch(_) { this.showToast('Failed to delete project', 'error') }
    },

    async removeFromProject(projectId, soundId) {
      try {
        const r = await fetch(`/api/projects/${projectId}/sounds/${soundId}`, { method: 'DELETE' })
        if (r.ok) {
          projWsDestroySingle(soundId)
          this.projectSounds = this.projectSounds.filter(s => s.id !== soundId)
          await this.loadProjects()
        }
      } catch(_) { this.showToast('Failed to remove sound', 'error') }
    },

    projTogglePlay(sound) {
      projWsToggle(sound)
    },

    // Lazy-load mini waveform for library row (called on mouseenter of the wave div)
    miniWsLoad(sound) {
      miniWsLoad(sound)
    },

    /* ── Indexing ─────────────────────────────────────────────── */
    _nextFolderUid: 1,

    addIndexFolder() {
      this.indexFolders.push({ path: '', source: 'sfx', _uid: ++this._nextFolderUid })
    },

    removeIndexFolder(i) {
      this.indexFolders.splice(i, 1)
      if (!this.indexFolders.length) this.indexFolders = [{ path: '', source: 'sfx', _uid: ++this._nextFolderUid }]
    },

    async startReindex() {
      if (this.indexing) return   // no double starts
      const validFolders = this.indexFolders.filter(f => f.path.trim())
      if (!validFolders.length) {
        this.showToast('Enter at least one folder path', 'error')
        return
      }
      this.showIndexModal  = false
      this.indexing        = true
      this.indexSummary    = null
      this.indexCancelConfirm = false
      this.indexCancelling = false
      this.indexErrorsOpen = false
      this.indexProgress   = { done: 0, total: 0, status: 'indexing', phase: 'scanning', total_known: false,
                               discovered: 0, roots_total: validFolders.length, roots_done: 0,
                               recent_errors: [], errors: 0, skipped: 0, can_cancel: true }

      try {
        const r = await fetch('/api/index/start', {
          method:  'POST',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ folders: validFolders }),
        })
        if (r.status === 409) {
          this.showToast('Indexing is already running — showing its progress', 'error')
        } else if (!r.ok) {
          throw new Error('HTTP ' + r.status)
        }
      } catch (e) {
        this.showToast('Failed to start index', 'error')
        this.indexing = false
        return
      }
      this._watchIndex()
    },

    // Follow /api/index/progress (SSE, or the polling shim in the desktop webview).
    _watchIndex() {
      if (this._indexEs) { try { this._indexEs.close() } catch (_) {} }
      const es = this._indexEs = new EventSource('/api/index/progress')
      es.onmessage = (e) => {
        try {
          const data = JSON.parse(e.data)
          this.indexProgress = data
          if (this._indexIsTerminal(data)) { es.close(); this._indexFinished(data) }
        } catch (_) {}
      }
      es.onerror = async () => {
        es.close()
        if (!this.indexing) return
        // Stream ended without a terminal event we saw: ask for the final snapshot once.
        try {
          const d = await (await fetch('/api/index/status', { cache: 'no-store' })).json()
          if (this._indexIsTerminal(d)) { this.indexProgress = d; this._indexFinished(d); return }
          if (d.status === 'indexing') { setTimeout(() => { if (this.indexing) this._watchIndex() }, 500); return }
        } catch (_) {}
        this.indexing = false
        this.indexCancelling = false
        this.showToast('Lost connection to the indexer', 'error')
      }
    },

    _indexIsTerminal(d) {
      const s = String((d && d.status) || '')
      return s === 'done' || s === 'cancelled' || s.startsWith('error')
    },

    _indexFinished(d) {
      this.indexing = false
      this.indexCancelling = false
      this.indexCancelConfirm = false
      this.indexSummary = d
      if (String(d.status).startsWith('error')) {
        this.showToast('Indexing error', 'error')
      } else {
        this.showToast(d.status === 'cancelled' ? 'Indexing cancelled' : `Indexed ${U.num(d.added != null ? d.added : d.done)} files`, 'success')
      }
      if ((d.added == null ? d.done : d.added) > 0) { this.search(); this.loadFolders(); this.loadRoots(); this.loadTags() }
    },

    indexAskCancel()  { if (this.indexing && !this.indexCancelling) this.indexCancelConfirm = true },
    indexKeepGoing()  { this.indexCancelConfirm = false },
    async indexStop() {
      this.indexCancelConfirm = false
      this.indexCancelling = true
      try {
        const r = await fetch('/api/index/cancel', { method: 'POST' })
        if (r.status === 409) {
          // Not running any more: the run just finished on its own — pick up the final snapshot.
          const d = await (await fetch('/api/index/status', { cache: 'no-store' })).json()
          if (this._indexIsTerminal(d)) { this.indexProgress = d; this._indexFinished(d) }
          else this.indexCancelling = false
        }
      } catch (_) {
        this.indexCancelling = false
        this.showToast('Could not send cancel request', 'error')
      }
    },
    indexDismiss() { this.indexSummary = null; this.indexErrorsOpen = false },

    get indexProgressPct() {
      const p = this.indexProgress
      if (!p || !p.total_known || !p.total) return 0
      return Math.min(100, Math.floor((p.done / p.total) * 100))
    },

    get indexPhaseLabel() {
      if (this.indexCancelling) return 'Cancelling…'
      switch (this.indexProgress.phase) {
        case 'scanning':   return 'Scanning folders…'
        case 'reading':    return 'Reading audio details…'
        case 'writing':    return 'Saving to library…'
        case 'finalizing': return 'Finishing up…'
        default:           return 'Starting…'
      }
    },
    get indexIndeterminate() { return this.indexing && !this.indexProgress.total_known },
    get indexFolderStep() {
      const p = this.indexProgress
      if (!p.roots_total) return ''
      return `Folder ${Math.min(p.roots_done + 1, p.roots_total)} of ${p.roots_total}`
    },
    get indexFileName() {
      const c = this.indexProgress.current_path || ''
      return c.split('/').pop() || ''
    },
    get indexEta()   { return this.indexProgress.phase === 'scanning' ? '' : U.etaText(this.indexProgress.eta_ms) },
    get indexSpeed() {
      const f = this.indexProgress.files_per_sec
      return f > 0 ? `${f >= 10 ? Math.round(f) : f.toFixed(1)} files/s` : ''
    },
    get indexSummaryText() {
      const d = this.indexSummary
      if (!d) return ''
      const added = d.added != null ? d.added : d.done
      if (d.status === 'cancelled') {
        return `Cancelled — ${U.num(added)} file${added === 1 ? '' : 's'} added, your existing library is unchanged`
      }
      if (String(d.status).startsWith('error')) return `Indexing stopped: ${d.message || d.status.replace(/^error:\s*/, '')}`
      let t = `Indexed ${U.num(added)} files in ${U.spoken(d.elapsed_ms)}`
      if (d.skipped) t += ` — ${U.num(d.skipped)} skipped`
      return t
    },

    /* ── Tags ─────────────────────────────────────────────────── */
    toggleTagFilter(tag) {
      const idx = this.filterTags.indexOf(tag)
      if (idx >= 0) this.filterTags.splice(idx, 1)
      else          this.filterTags.push(tag)
      this.search()
    },

    isTagActive(tag) {
      return this.filterTags.includes(tag)
    },

    /* ── Bundles ──────────────────────────────────────────────── */
    async createBundle() {
      if (!this.newBundleName.trim()) return
      try {
        const r = await fetch('/api/bundles', {
          method:  'POST',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ name: this.newBundleName.trim() }),
        })
        if (!r.ok) throw new Error(r.statusText)
        const b = await r.json()
        this.bundles.push(b)
        this.newBundleName    = ''
        this.showCreateBundle = false
        this.showToast('Bundle created', 'success')
      } catch (e) {
        this.showToast('Failed to create bundle', 'error')
      }
    },

    async deleteBundle(bundle, event) {
      event.stopPropagation()
      if (!confirm(`Delete bundle "${bundle.name}"?`)) return
      const prevBundles = this.bundles.slice()
      this.bundles = this.bundles.filter(b => b.id !== bundle.id)
      const prevFilterBundle = this.filterBundle
      if (this.filterBundle === bundle.id) {
        this.filterBundle = null
      }
      try {
        const r = await fetch(`/api/bundles/${bundle.id}`, { method: 'DELETE' })
        if (!r.ok) throw new Error(r.statusText)
        if (prevFilterBundle === bundle.id) this.search()
      } catch (e) {
        // Roll back optimistic removal
        this.bundles      = prevBundles
        this.filterBundle = prevFilterBundle
        this.showToast('Failed to delete bundle', 'error')
      }
    },

    selectBundle(id) {
      this.filterBundle = this.filterBundle === id ? null : id
    },

    /* ── Radar ────────────────────────────────────────────────── */
    async pollRadarStatus() {
      try {
        const r    = await fetch('/api/radar/status')
        const data = await r.json()
        this.radarStatus   = data.status
        this.radarProgress = data.progress || { done: 0, total: 0 }
        this.radarError    = data.state === 'error' ? (data.error || 'Unknown error') : ''
        // A failed run must not look like it is still running
        if (this.radarError && (this.radarStatus === 'generating' || this.radarStatus === 'computing')) {
          this.radarStatus = 'not_started'
          this.radarGenerating = false
        }
        // After a model switch only current-model embeddings count
        this.radarIncomplete = this.radarStatus !== 'ready' &&
          this.radarProgress.total > 0 && this.radarProgress.done < this.radarProgress.total
        if (data.status === 'ready' && this.activeTab === 'radar') {
          // $nextTick ensures Alpine has finished showing #radar-tab before
          // radar.js tries to measure the canvas dimensions via getBoundingClientRect.
          this.$nextTick(() => document.dispatchEvent(new CustomEvent('show-radar')))
        }
      } catch (_) {}
      const isActive = this.radarStatus === 'generating' || this.radarStatus === 'computing'
      this.radarPollInterval = setTimeout(() => this.pollRadarStatus(), isActive ? 2000 : 10000)
    },

    async generateRadar() {
      if (this.radarGenerating) return
      clearTimeout(this.waitForReadyTimeout)
      this.waitForReadyTimeout = null
      if (this.$store.models.isMissing('clap')) {
        this.$store.models.openPanel()
        return
      }
      this.radarGenerating = true
      this.radarError = ''
      this.radarStatus = 'generating'
      try {
        const gr = await fetch('/api/radar/generate', { method: 'POST' })
        if (gr.status === 409) {
          // model_not_ready — the fetch helper already refreshed model state
          this.radarStatus = 'not_started'
          this.radarGenerating = false
          return
        }
        if (!gr.ok) throw new Error(gr.statusText)
        const waitForReady = async () => {
          if (this.radarStatus === 'ready') {
            this.waitForReadyTimeout = null
            this.radarGenerating = false
            this.$nextTick(() => {
              document.dispatchEvent(new CustomEvent('radar-invalidate'))
              document.dispatchEvent(new CustomEvent('show-radar'))
            })
            return
          }
          if (this.radarStatus !== 'generating' && this.radarStatus !== 'computing') {
            this.waitForReadyTimeout = null
            this.radarGenerating = false
            return
          }
          this.waitForReadyTimeout = setTimeout(waitForReady, 2000)
        }
        waitForReady()
      } catch (err) {
        this.radarStatus = 'not_started'
        this.radarError = err.message || 'Could not start'
        this.radarGenerating = false
      }
    },

    async toggleRadar() {
      this.showRadar = !this.showRadar
      if (this.showRadar && this.radarStatus === 'ready') {
        this.$nextTick(() => document.dispatchEvent(new CustomEvent('show-radar')))
      }
    },

    toggleAutoplay() {
      this.autoplay = !this.autoplay
      localStorage.setItem('daouchi_autoplay', this.autoplay)
      document.dispatchEvent(new CustomEvent('autoplay-changed', { detail: this.autoplay }))
    },

    toggleTheme() {
      this.themeLight = !this.themeLight
      if (this.themeLight) {
        document.documentElement.setAttribute('data-theme', 'light')
      } else {
        document.documentElement.removeAttribute('data-theme')
      }
      localStorage.setItem('daouchi_theme', this.themeLight ? 'light' : 'dark')
    },

    /* ── Command palette ──────────────────────────────────────── */
    openCmdPalette() {
      this.showCmdPalette = true
      this.cmdQuery = ''
      this.cmdActiveIdx = 0
      this.$nextTick(() => {
        const el = document.getElementById('cmd-input')
        if (el) el.focus()
      })
    },

    closeCmdPalette() {
      this.showCmdPalette = false
    },

    get cmdCommands() {
      const base = [
        { icon: '⭐', label: 'Show Favorites',        shortcut: '',      action: () => { this.filterFavorite = true; this.search() } },
        { icon: '✕',  label: 'Clear All Filters',     shortcut: '',      action: () => this.resetFilters() },
        { icon: '♪',  label: 'Filter: WAV files',                        action: () => { this.filterExt = 'wav'; this.search() } },
        { icon: '♪',  label: 'Filter: MP3 files',                        action: () => { this.filterExt = 'mp3'; this.search() } },
        { icon: '🔊', label: 'Filter: SFX',                              action: () => { this.filterSource = 'sfx'; this.search() } },
        { icon: '♫',  label: 'Filter: Music',                            action: () => { this.filterSource = 'music'; this.search() } },
        { icon: '≋',  label: 'Toggle Semantic Mode',  shortcut: 'Alt+S', action: () => { this.semanticMode = !this.semanticMode; this.search() } },
        { icon: '✦',  label: 'AI Models…',                               action: () => this.$store.models.openPanel() },
        { icon: '⊙',  label: 'Open Acoustic Radar',                      action: () => { if (!this.showRadar) this.toggleRadar() } },
        { icon: '↺',  label: 'Reindex Library',                          action: () => { this.showIndexModal = true } },
        { icon: '▶',  label: 'Toggle Autoplay',                          action: () => this.toggleAutoplay() },
        { icon: '◧',  label: 'Switch to Library',     shortcut: 'Cmd+L', action: () => this.switchTab('library') },
        { icon: '◫',  label: 'Switch to Dashboard',   shortcut: 'Cmd+D', action: () => this.switchTab('dashboard') },
        { icon: '◨',  label: 'Switch to Radar',       shortcut: 'Cmd+R', action: () => this.switchTab('radar') },
        { icon: '📁', label: 'Switch to Projects',    shortcut: 'Cmd+P', action: () => this.switchTab('projects') },
      ]
      // Add folder commands
      const folderCmds = (this.folderTree || []).map(f => ({
        icon: '📁', label: 'Folder: ' + f.name, action: () => this.selectFolder(f.path)
      }))
      // Add tag commands
      const tagCmds = (this.allTags || []).slice(0, 20).map(t => ({
        icon: '#', label: 'Tag: ' + t, action: () => { this.filterTags = [t]; this.search() }
      }))
      return [...base, ...folderCmds, ...tagCmds]
    },

    get cmdFiltered() {
      if (!this.cmdQuery.trim()) return this.cmdCommands.slice(0, 8)
      const q = this.cmdQuery.toLowerCase()
      return this.cmdCommands.filter(c => c.label.toLowerCase().includes(q)).slice(0, 10)
    },

    cmdExecute(cmd) {
      if (!cmd) cmd = this.cmdFiltered[this.cmdActiveIdx]
      if (!cmd) return
      this.closeCmdPalette()
      cmd.action()
    },

    cmdKeydown(e) {
      const items = this.cmdFiltered
      if (e.key === 'ArrowDown') { e.preventDefault(); this.cmdActiveIdx = Math.min(this.cmdActiveIdx + 1, items.length - 1) }
      if (e.key === 'ArrowUp')   { e.preventDefault(); this.cmdActiveIdx = Math.max(this.cmdActiveIdx - 1, 0) }
      if (e.key === 'Enter')     { e.preventDefault(); this.cmdExecute() }
      if (e.key === 'Escape')    { this.closeCmdPalette() }
      // Reset active index on query change
      if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') this.cmdActiveIdx = 0
    },

    /* ── Format helpers ───────────────────────────────────────── */
    timeAgo(dateStr) {
      if (!dateStr) return ''
      const d = new Date(dateStr)
      const diff = (Date.now() - d.getTime()) / 1000
      if (diff < 60)    return 'just now'
      if (diff < 3600)  return Math.floor(diff/60) + 'm ago'
      if (diff < 86400) return Math.floor(diff/3600) + 'h ago'
      if (diff < 7*86400) return Math.floor(diff/86400) + 'd ago'
      return d.toLocaleDateString()
    },

    formatDate(dateStr) {
      if (!dateStr) return ''
      return new Date(dateStr).toLocaleString()
    },

    formatDuration(secs)  { return window.U.duration(secs) },
    formatSize(bytes)     { return window.U.size(bytes) },
    badgeClass(ext)       { return window.U.badgeClass(ext) },

    /* ── Toast ────────────────────────────────────────────────── */
    showToast(msg, type = 'success') {
      this.toast = { msg, type }
      clearTimeout(this._toastTimer)
      this._toastTimer = setTimeout(() => { this.toast = null }, 2500)
    },

    /* ── Audio FX preset ──────────────────────────────────────── */
    setActiveFX(preset) {
      this.activeFX = preset
      document.dispatchEvent(new CustomEvent('fx-preset-changed', { detail: preset }))
    },

    /* ── Reset filters ────────────────────────────────────────── */
    resetFilters() {
      this.filterSource     = ''
      this.filterExt        = ''
      this.filterDurMin     = null
      this.filterDurMax     = null
      this.filterFavorite   = false
      this.filterLabel      = 0
      this.filterMinRating  = 0
      this.filterTags       = []
      this.filterBundle     = null
      this.activeFolderPath = null
      document.dispatchEvent(new CustomEvent('radar-folder-filter', { detail: null }))
      this.search()
    },

    /* ── Flat folder list for sidebar tree ────────────────────── */
    flattenTree(nodes, depth = 0) {
      const out = []
      for (const n of (nodes || [])) {
        out.push({ ...n, depth })
        if (this.folderExpanded[n.path] && n.children && n.children.length) {
          out.push(...this.flattenTree(n.children, depth + 1))
        }
      }
      return out
    },
  }
}

/* ── Project row WaveSurfer manager ───────────────────────────────────────
 * Manages one WaveSurfer instance per sound row in the Projects tab.
 * All instances are lazy-loaded (created when the row appears) and
 * only one plays at a time.
 * ─────────────────────────────────────────────────────────────────────── */
const _projWs      = {}   // soundId → WaveSurfer
const _projPlaying = { id: null }

function projWsInitAll(sounds) {
  if (!window.WaveSurfer) return
  sounds.forEach(sound => {
    const container = document.getElementById('proj-wave-' + sound.id)
    if (!container || _projWs[sound.id]) return
    try {
      const ws = WaveSurfer.create({
        container,
        waveColor:     '#3a3a3a',
        progressColor: '#e47a2a',
        cursorColor:   'transparent',
        barWidth:      2,
        barGap:        1,
        height:        44,
        normalize:     true,
        interact:      true,
      })
      ws.load('/audio/stream?path=' + encodeURIComponent(sound.path))
      ws.on('play',   () => _projSetPlayIcon(sound.id, true))
      ws.on('pause',  () => _projSetPlayIcon(sound.id, false))
      ws.on('finish', () => { _projSetPlayIcon(sound.id, false); _projPlaying.id = null })
      _projWs[sound.id] = ws
    } catch(_) {}
  })
}

function projWsToggle(sound) {
  const ws = _projWs[sound.id]
  if (!ws) {
    // Not yet loaded — create on demand then play
    const container = document.getElementById('proj-wave-' + sound.id)
    if (!container || !window.WaveSurfer) return
    try {
      const newWs = WaveSurfer.create({
        container,
        waveColor:     '#3a3a3a',
        progressColor: '#e47a2a',
        cursorColor:   'transparent',
        barWidth:      2,
        barGap:        1,
        height:        44,
        normalize:     true,
        interact:      true,
      })
      newWs.load('/audio/stream?path=' + encodeURIComponent(sound.path))
      newWs.on('ready',  () => { newWs.play() })
      newWs.on('play',   () => _projSetPlayIcon(sound.id, true))
      newWs.on('pause',  () => _projSetPlayIcon(sound.id, false))
      newWs.on('finish', () => { _projSetPlayIcon(sound.id, false); _projPlaying.id = null })
      _projWs[sound.id] = newWs
    } catch(_) {}
    return
  }

  // Stop any other playing instance first
  if (_projPlaying.id !== null && _projPlaying.id !== sound.id) {
    const other = _projWs[_projPlaying.id]
    if (other) { try { other.pause() } catch(_) {} }
    _projSetPlayIcon(_projPlaying.id, false)
  }

  if (ws.isPlaying()) {
    ws.pause()
    _projPlaying.id = null
  } else {
    ws.play()
    _projPlaying.id = sound.id
  }
}

function _projSetPlayIcon(soundId, playing) {
  _libSetPlayIcon(soundId, playing)
}

function projWsDestroy() {
  Object.values(_projWs).forEach(ws => { try { ws.destroy() } catch(_) {} })
  Object.keys(_projWs).forEach(k => delete _projWs[k])
  _projPlaying.id = null
}

function projWsDestroySingle(soundId) {
  const ws = _projWs[soundId]
  if (ws) { try { ws.destroy() } catch(_) {} }
  delete _projWs[soundId]
  if (_projPlaying.id === soundId) _projPlaying.id = null
}

/* ── Mini-waveform manager for Library rows ─────────────────────────────── */
const _miniWs    = {}   // soundId → WaveSurfer
const _libPlaying = { id: null }

// Exposed to Alpine templates via @mouseenter / direct call
function miniWsLoad(sound) {
  if (_miniWs[sound.id] || !window.WaveSurfer) return
  const el = document.getElementById('mw-' + sound.id)
  if (!el) return
  try {
    const ws = WaveSurfer.create({
      container:     el,
      waveColor:     'rgba(255,255,255,0.25)',
      progressColor: '#e47a2a',
      cursorColor:   'transparent',
      barWidth:      2,
      barGap:        1,
      barRadius:     1,
      height:        24,
      normalize:     true,
      interact:      false,
    })
    ws.load('/audio/stream?path=' + encodeURIComponent(sound.path))
    ws.on('finish', () => {
      _libSetPlayIcon(sound.id, false)
      _libPlaying.id = null
    })
    _miniWs[sound.id] = ws
  } catch(_) {}
}

const _SVG_PLAY  = '<svg class="lib-play-icon" width="13" height="13" viewBox="0 0 24 24" fill="currentColor"><polygon points="5 3 19 12 5 21 5 3"/></svg>'
const _SVG_PAUSE = '<svg class="lib-play-icon" width="13" height="13" viewBox="0 0 24 24" fill="currentColor"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg>'

function _libSetPlayIcon(soundId, playing) {
  const btn = document.getElementById('row-play-' + soundId)
  if (!btn) return
  // artwork-play-btn icons are driven by Alpine's playingId — skip direct DOM update
  if (btn.classList.contains('artwork-play-btn')) return
  btn.innerHTML = playing ? _SVG_PAUSE : _SVG_PLAY
  btn.classList.toggle('playing', playing)
}

// Expose unified row play handler for both library and project contexts
window.rowTogglePlay = function (soundId) {
  const app  = document.getElementById('app')
  const data = app && app._x_dataStack && app._x_dataStack[0]
  if (!data) return
  const sound = (data.results || []).find(s => s.id === soundId)
    || (data.projectSounds || []).find(s => s.id === soundId)
  if (sound) data.libRowTogglePlay.call(data, sound)
}

window.rowCycleLabel = function (soundId) {
  const app  = document.getElementById('app')
  const data = app && app._x_dataStack && app._x_dataStack[0]
  if (!data) return
  const sound = (data.results || []).find(s => s.id === soundId)
    || (data.projectSounds || []).find(s => s.id === soundId)
  if (sound) data.cycleLabel.call(data, sound)
}

// Keep row icon in sync with main player play/pause state.
let _playerStateSoundId = null
document.addEventListener('player-playstate', (e) => {
  const { soundId, playing } = e.detail
  // Update Alpine reactive playingId so artwork buttons re-render via x-show
  const appEl = document.getElementById('app')
  const data = appEl && appEl._x_dataStack && appEl._x_dataStack[0]
  if (data) data.playingId = playing ? soundId : null
  // Clear previous legacy standalone button
  if (_playerStateSoundId && _playerStateSoundId !== soundId) {
    _libSetPlayIcon(_playerStateSoundId, false)
  }
  _playerStateSoundId = playing ? soundId : null
  _libSetPlayIcon(soundId, playing)
})

/* ── Row action helpers ─────────────────────────────────────────────────────── */

// Copy file path to clipboard with temporary icon feedback
function rowCopyPath(sound, event) {
  if (!sound) return
  const btn = event.currentTarget
  navigator.clipboard.writeText(sound.path).then(() => {
    btn.classList.add('copied')
    clearTimeout(btn._ct)
    btn._ct = setTimeout(() => btn.classList.remove('copied'), 1500)
  }).catch(() => {})
}
window.rowCopyPath = rowCopyPath

// Reveal file in Finder (the server resolves the sound id to its path)
async function rowReveal(sound) {
  if (!sound) return
  const app = document.querySelector('[x-data]')?._x_dataStack?.[0]
  if (app) await app._reveal({ kind: 'sound', id: sound.id })
}
window.rowReveal = rowReveal

// Delete sound: opens the same remove-from-library confirmation as folders (Trash is opt-in there)
async function rowDelete(sound) {
  if (!sound) return
  const app = document.querySelector('[x-data]')?._x_dataStack?.[0]
  if (app) app.openRemoveSound(sound)
}
window.rowDelete = rowDelete

// Rename / move modal
let _renameTarget = null
function rowRename(sound) {
  _renameTarget = sound
  document.getElementById('rename-modal-title').textContent = 'Rename / Move: ' + sound.filename
  document.getElementById('rename-input').value = sound.filename
  document.getElementById('rename-folder-input').value = (sound.folder_path || '').replace(/\/[^/]+$/, '') || sound.folder_path || ''
  document.getElementById('rename-error').textContent = ''
  document.getElementById('rename-modal').style.display = 'flex'
  setTimeout(() => document.getElementById('rename-input').select(), 50)
}
window.rowRename = rowRename

function closeRenameModal() {
  document.getElementById('rename-modal').style.display = 'none'
  _renameTarget = null
}
window.closeRenameModal = closeRenameModal

async function submitRename() {
  if (!_renameTarget) return
  const newName   = document.getElementById('rename-input').value.trim()
  const newFolder = document.getElementById('rename-folder-input').value.trim()
  const errEl     = document.getElementById('rename-error')

  if (!newName) { errEl.textContent = 'Filename cannot be empty'; return }

  const body = {}
  if (newFolder) {
    // Move: combine folder + filename into full path
    body.new_path = newFolder.replace(/\/$/, '') + '/' + newName
  } else {
    body.new_name = newName
  }

  try {
    const r = await fetch(`/api/sounds/${_renameTarget.id}/rename`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    })
    const data = await r.json()
    if (!r.ok) { errEl.textContent = data || 'Rename failed'; return }

    // Update in Alpine results list
    const app = document.querySelector('[x-data]')?._x_dataStack?.[0]
    if (app) {
      const idx = app.results.findIndex(s => s.id === _renameTarget.id)
      if (idx >= 0) {
        app.results[idx].filename    = data.filename
        app.results[idx].path        = data.path
        app.results[idx].folder_path = data.folder_path
      }
      if (app.selectedSound && app.selectedSound.id === _renameTarget.id) {
        app.selectedSound.filename    = data.filename
        app.selectedSound.path        = data.path
        app.selectedSound.folder_path = data.folder_path
      }
    }
    closeRenameModal()
  } catch(e) { errEl.textContent = 'Error: ' + e.message }
}
window.submitRename = submitRename
