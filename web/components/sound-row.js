;(function () {
  'use strict'

  /**
   * Returns an <img> string for album artwork.
   * Hidden until the image loads; self-hides on error.
   * @param {number} soundId
   * @param {boolean} [small]  use row-artwork-sm size (26×26)
   */
  window.artworkImg = function (soundId, small) {
    const cls = 'row-artwork' + (small ? ' row-artwork-sm' : '')
    return `<img class="${cls}" src="/api/sounds/${soundId}/artwork" alt=""` +
      ` onerror="this.style.display='none'"` +
      ` onload="this.style.display='block'"` +
      ` style="display:none;">`
  }

  /**
   * Returns artwork thumbnail with play button overlaid and label pip in corner.
   * The play button sits centered over the art; semi-transparent by default,
   * fully opaque on row hover / when playing.
   * The label pip (colored dot) sits bottom-left; only shown when label > 0.
   */
  window.artworkWithPlay = function (sound, small) {
    const LABEL_COLORS = ['', '#ff3b30', '#ff9f0a', '#ffd60a', '#30d158', '#0a84ff']
    const size  = small ? 28 : 44
    const iconW = small ? 10 : 14
    const br    = small ? 3 : 4
    const label = sound.label || 0
    const labelColor = LABEL_COLORS[label] || ''

    const img = `<img src="/api/sounds/${sound.id}/artwork" alt=""` +
      ` onerror="this.style.display='none'"` +
      ` onload="this.style.display='block'"` +
      ` style="display:none;position:absolute;inset:0;width:100%;height:100%;object-fit:cover;">`

    const btn = `<button class="artwork-play-btn" id="row-play-${sound.id}"` +
      ` onclick="event.stopPropagation();window.rowTogglePlay(${sound.id})"` +
      ` title="Play / Pause">` +
      `<svg width="${iconW}" height="${iconW}" viewBox="0 0 24 24" fill="currentColor">` +
      `<polygon points="5 3 19 12 5 21 5 3"/></svg></button>`

    const pip = label > 0
      ? `<span style="position:absolute;bottom:2px;left:2px;width:7px;height:7px;` +
        `border-radius:50%;background:${labelColor};border:1px solid rgba(255,255,255,0.5);` +
        `pointer-events:none;z-index:3;display:block;"></span>` +
        `<button onclick="event.stopPropagation();window.rowCycleLabel(${sound.id})" title="Cycle label"` +
        ` style="position:absolute;inset:0;background:transparent;border:none;cursor:pointer;z-index:2;"></button>`
      : ''

    return `<div class="artwork-wrap" style="width:${size}px;height:${size}px;border-radius:${br}px;">${img}${btn}${pip}</div>`
  }

  /**
   * Returns the filename + extension badge HTML (flex row).
   * Suitable for <div x-html="soundNameBadge(sound)">.
   */
  window.soundNameBadge = function (sound) {
    const E   = window.U.esc
    const ext = sound.extension || '?'
    const name = E(sound.filename || '')
    const badgeCls = window.U.badgeClass(ext)
    return `<span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${name}</span>` +
      `<span class="${badgeCls}" style="flex-shrink:0;font-size:9px;padding:1px 5px;">${E(ext.toUpperCase())}</span>`
  }

  /**
   * Returns a folder-sub <span> or empty string if no subfolder.
   */
  window.soundSubfolder = function (sound) {
    const sub = sound.folder_path || sound.collection || ''
    return sub ? `<span class="folder-sub">${window.U.esc(sub)}</span>` : ''
  }

  /**
   * Returns a result-tags <div> with up to `max` (default 2) tags,
   * or empty string when there are no tags.
   */
  window.soundTagsHTML = function (sound, max) {
    const tags = (sound.tags || []).slice(0, max || 2)
    if (!tags.length) return ''
    const chips = tags.map(t => `<span class="result-tag">${window.U.esc(t)}</span>`).join('')
    return `<div class="result-tags">${chips}</div>`
  }
  /**
   * Returns a play/pause button HTML string.
   * The button calls window.rowTogglePlay(soundId) on click.
   * JS in player.js swaps the icon between ▶ and ⏸ via id="row-play-{id}".
   */
  window.rowPlayBtn = function (sound) {
    return `<button class="lib-play-btn" id="row-play-${sound.id}"` +
      ` onclick="event.stopPropagation();window.rowTogglePlay(${sound.id})"` +
      ` title="Play / Pause">` +
      `<svg class="lib-play-icon" width="13" height="13" viewBox="0 0 24 24" fill="currentColor">` +
      `<polygon points="5 3 19 12 5 21 5 3"/></svg></button>`
  }

})()
