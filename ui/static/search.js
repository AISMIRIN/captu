// Search page: filter state management, tag chips, session restore.

// Pending state to restore ep/sub after the episodes fragment loads.
var _restoreState = null;

// ── Active tag chips ──────────────────────────────────────────────────────────
var activeTags = [];

// Re-render the chip strip and update the hidden input.
function renderTagChips() {
  var container = document.getElementById('tag-active');
  var hidden = document.getElementById('active-tags');
  if (!container) return;
  container.innerHTML = '';
  activeTags.forEach(function(t) {
    var chip = document.createElement('span');
    chip.className = 'inline-flex items-center gap-1 px-2 py-0.5 rounded text-xs bg-blue-700 text-white';
    chip.innerHTML =
      '<span>' + t.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;') + '</span>' +
      '<button type="button" class="text-blue-200 hover:text-white leading-none" aria-label="タグフィルタを外す">×</button>';
    chip.querySelector('button').addEventListener('click', function() { removeTagFilter(t); });
    container.appendChild(chip);
  });
  if (hidden) hidden.value = activeTags.join('\n');
}

function addTagFilter(v) {
  if (!v || activeTags.indexOf(v) !== -1) return;
  activeTags.push(v);
  renderTagChips();
  triggerSearch();
}

function removeTagFilter(v) {
  activeTags = activeTags.filter(function(t) { return t !== v; });
  renderTagChips();
  triggerSearch();
}

// ── Tab switching ─────────────────────────────────────────────────────────────
// noSearch: skip triggerSearch (used during state restoration).
function setFilter(value, noSearch) {
  document.getElementById('active-filter').value = value;
  document.querySelectorAll('.filter-tab').forEach(function(btn) {
    var active = btn.dataset.filter === value;
    btn.classList.toggle('bg-blue-600', active);
    btn.classList.toggle('text-white', active);
    btn.classList.toggle('bg-gray-700', !active);
    btn.classList.toggle('text-gray-300', !active);
  });
  if (!noSearch) triggerSearch();
}

// ── Save current search state to sessionStorage ───────────────────────────────
function saveSearchState() {
  var q = document.querySelector('[name="q"]');
  var programSelect = document.getElementById('program-select');
  var epSel = document.querySelector('#ep-or-sub [name="ep"]');
  var subSel = document.querySelector('#ep-or-sub [name="sub"]');
  var dateFrom = document.getElementById('date-from');
  var dateTo = document.getElementById('date-to');
  var filter = document.getElementById('active-filter');
  sessionStorage.setItem('captu_search', JSON.stringify({
    q: q ? q.value : '',
    program_id: programSelect ? programSelect.value : '',
    ep: epSel ? epSel.value : '',
    sub: subSel ? subSel.value : '',
    date_from: dateFrom ? dateFrom.value : '',
    date_to: dateTo ? dateTo.value : '',
    filter: filter ? filter.value : 'all',
    tags: activeTags
  }));
}

// ── Trigger search via dosearch custom event (avoids keyup[key] filter issue) ─
function triggerSearch() {
  saveSearchState();
  var input = document.querySelector('[name="q"]');
  if (input) htmx.trigger(input, 'dosearch');
}

// ── Handle tag-filter select: add selected tag as chip, reset picker ──────────
document.addEventListener('change', function(e) {
  if (e.target.id === 'tag-filter') {
    // Add value as active chip; ignore the placeholder (empty value).
    if (e.target.value) addTagFilter(e.target.value);
    e.target.value = '';
    return;
  }
  if (e.target.id === 'program-select') return; // htmx handles /api/episodes swap
  if (e.target.closest('#filter-bar')) {
    triggerSearch();
  }
});

// ── After htmx swaps #ep-or-sub: restore state, search ───────────────────────
document.body.addEventListener('htmx:afterSwap', function(e) {
  if (e.target.id === 'ep-or-sub') {
    // Attach change listeners to episode or subtitle selector.
    e.target.querySelectorAll('select').forEach(function(sel) {
      sel.addEventListener('change', triggerSearch);
    });

    // Restore ep/sub from saved state if present.
    if (_restoreState) {
      var s = _restoreState;
      _restoreState = null;
      var epSel = e.target.querySelector('[name="ep"]');
      if (epSel && s.ep) epSel.value = s.ep;
      var subSel = e.target.querySelector('[name="sub"]');
      if (subSel && s.sub) subSel.value = s.sub;
    }
    triggerSearch();
  }
});

// ── Restore search state from sessionStorage on page load ─────────────────────
document.addEventListener('DOMContentLoaded', function() {
  var saved = sessionStorage.getItem('captu_search');
  if (!saved) return;
  var state;
  try { state = JSON.parse(saved); } catch(e) { return; }

  // Restore text input.
  var qInput = document.querySelector('[name="q"]');
  if (qInput && state.q) qInput.value = state.q;

  // Restore filter tab without triggering search yet.
  if (state.filter) setFilter(state.filter, true);

  // Restore permanent date fields immediately.
  var df = document.getElementById('date-from');
  if (df && state.date_from) df.value = state.date_from;
  var dt = document.getElementById('date-to');
  if (dt && state.date_to) dt.value = state.date_to;

  // Restore active tag chips (no need to wait for options; chips reconstruct from saved array).
  if (Array.isArray(state.tags) && state.tags.length) {
    activeTags = state.tags;
    renderTagChips();
  }

  // Restore program selector.
  var programSelect = document.getElementById('program-select');
  if (programSelect && state.program_id) {
    programSelect.value = state.program_id;
    // Load episodes/subtitle fragment; afterSwap will restore ep/sub and fire search.
    _restoreState = state;
    htmx.ajax('GET', '/api/episodes', {
      target: '#ep-or-sub',
      values: { program_id: state.program_id }
    });
  } else {
    // No program — fire search directly.
    triggerSearch();
  }

  // Save state right before any page navigation so debounce timing doesn't lose q value.
  window.addEventListener('beforeunload', saveSearchState);
});
