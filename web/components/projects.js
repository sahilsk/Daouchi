;(function() {
  'use strict'

  let _activeProjectId = null

  function escHtml(s) {
    return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;')
  }

  function renderProjectsSidebar(projects) {
    const el = document.getElementById('projects-sidebar')
    if (!el) return
    if (!projects || !projects.length) {
      el.innerHTML = '<div style="font-size:11px;color:var(--muted);padding:4px 0;">No projects yet.</div>' +
        '<button class="new-project-btn" onclick="window.daouchi_newProject()">+ New Project</button>'
      return
    }
    el.innerHTML = projects.map(p => `
      <div class="project-item ${_activeProjectId === p.id ? 'active' : ''}"
           onclick="window.daouchi_openProject(${p.id})" style="--project-color:${escHtml(p.color)}">
        <span class="project-dot"></span>
        <span class="project-name">${escHtml(p.name)}</span>
        <span class="project-count">${p.sound_count}</span>
        <button class="project-delete-btn" onclick="event.stopPropagation();window.daouchi_deleteProject(${p.id},${JSON.stringify(p.name)})">×</button>
      </div>
    `).join('') + '<button class="new-project-btn" onclick="window.daouchi_newProject()">+ New Project</button>'
  }

  window._renderProjectsSidebar = renderProjectsSidebar

  window.projectsInit = function() {
    const el = document.getElementById('projects-sidebar')
    if (!el) return
    const alpine = document.getElementById('app') && document.getElementById('app')._x_dataStack
    if (alpine && alpine[0] && alpine[0].projects) {
      renderProjectsSidebar(alpine[0].projects)
    }
  }

  window.daouchi_newProject = function() {
    const alpine = _getAlpineData()
    if (alpine && typeof alpine.newProject === 'function') {
      alpine.newProject()
    }
  }

  window.daouchi_deleteProject = async function(id, name) {
    if (!confirm('Delete project "' + name + '"?')) return
    try {
      await fetch('/api/projects/' + id, { method: 'DELETE' })
      if (_activeProjectId === id) {
        _activeProjectId = null
        document.dispatchEvent(new CustomEvent('project-closed'))
      }
      const alpine = _getAlpineData()
      if (alpine) await alpine.loadProjects()
    } catch(_) {}
  }

  window.daouchi_openProject = async function(id) {
    _activeProjectId = id
    const alpine = _getAlpineData()
    if (alpine && alpine.projects) renderProjectsSidebar(alpine.projects)
    try {
      const r = await fetch('/api/projects/' + id + '/sounds')
      if (!r.ok) return
      const sounds = await r.json()
      document.dispatchEvent(new CustomEvent('project-sounds-loaded', { detail: { sounds, projectId: id } }))
    } catch(_) {}
  }

  function _getAlpineData() {
    const el = document.getElementById('app')
    if (!el) return null
    try { return el._x_dataStack && el._x_dataStack[0] } catch(_) { return null }
  }

  function _showProjectsToast(msg) {
    const alpine = _getAlpineData()
    if (alpine && typeof alpine.showToast === 'function') {
      alpine.showToast(msg)
      return
    }
    let el = document.getElementById('projects-toast')
    if (!el) {
      el = document.createElement('div')
      el.id = 'projects-toast'
      el.style.cssText = 'position:fixed;bottom:60px;left:50%;transform:translateX(-50%);' +
        'background:rgba(20,20,20,.95);color:#e4e4e4;padding:8px 18px;border-radius:8px;' +
        'font-size:13px;z-index:9999;opacity:0;transition:opacity .2s;pointer-events:none;'
      document.body.appendChild(el)
    }
    el.textContent = msg
    el.style.opacity = '1'
    clearTimeout(el._t)
    el._t = setTimeout(function() { el.style.opacity = '0' }, 2500)
  }

  document.addEventListener('project-sounds-loaded', function(e) {
    const alpine = _getAlpineData()
    if (!alpine) return
    alpine.results = e.detail.sounds
    alpine.total = e.detail.sounds.length
    if (alpine.activeTab !== 'library') alpine.activeTab = 'library'
  })

})()
