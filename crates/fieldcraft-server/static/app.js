(function () {
  const app = document.querySelector('#app');
  const state = {
    token: localStorage.getItem('fieldcraft_token') || '',
    projectId: localStorage.getItem('fieldcraft_project') || '',
    authRequired: false,
    machineId: '',
    query: '',
    searchDraft: '',
    hits: [],
    selectedId: null,
    selected: null,
    loading: false,
  };

  const esc = (value) => String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');

  async function api(method, path, body) {
    const headers = {};
    if (state.token) headers.Authorization = `Bearer ${state.token}`;
    if (body !== undefined) headers['Content-Type'] = 'application/json';
    const response = await fetch(path, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    if (response.status === 401) {
      const error = new Error('Unauthorized');
      error.status = 401;
      throw error;
    }
    const data = await response.json();
    if (response.status >= 400) {
      const error = new Error(data.message || 'Request failed');
      error.status = response.status;
      throw error;
    }
    return data;
  }

  function inlineMd(text) {
    return esc(text)
      .replace(/\*\*(.+?)\*\*/g, '<strong>$1</strong>')
      .replace(/`(.+?)`/g, '<code>$1</code>');
  }

  function renderMd(markdown) {
    const lines = String(markdown || '').split('\n');
    let html = '';
    let inCode = false;
    let inList = false;
    for (const line of lines) {
      if (line.startsWith('```')) {
        if (inList) { html += '</ul>'; inList = false; }
        html += inCode ? '</code></pre>' : '<pre><code>';
        inCode = !inCode;
        continue;
      }
      if (inCode) {
        html += `${esc(line)}\n`;
        continue;
      }
      if (line.startsWith('### ')) {
        if (inList) { html += '</ul>'; inList = false; }
        html += `<h3>${inlineMd(line.slice(4))}</h3>`;
      } else if (line.startsWith('## ')) {
        if (inList) { html += '</ul>'; inList = false; }
        html += `<h2>${inlineMd(line.slice(3))}</h2>`;
      } else if (line.startsWith('# ')) {
        if (inList) { html += '</ul>'; inList = false; }
        html += `<h1>${inlineMd(line.slice(2))}</h1>`;
      } else if (/^- /.test(line)) {
        if (!inList) { html += '<ul>'; inList = true; }
        html += `<li>${inlineMd(line.slice(2))}</li>`;
      } else if (!line.trim()) {
        if (inList) { html += '</ul>'; inList = false; }
      } else {
        if (inList) { html += '</ul>'; inList = false; }
        html += `<p>${inlineMd(line)}</p>`;
      }
    }
    if (inList) html += '</ul>';
    if (inCode) html += '</code></pre>';
    return html;
  }

  function renderLogin(errorMessage = '') {
    app.innerHTML = `<div class="login-shell">
      <div class="login-card">
        <div class="app-label">Fieldcraft</div>
        <h1>Connect to lesson</h1>
        <p>Enter the API token for this server.</p>
        ${errorMessage ? `<div class="error-banner">${esc(errorMessage)}</div>` : ''}
        <input id="login-token" type="password" placeholder="fc_sk_..." autofocus />
        <button class="primary-button" style="width:100%" onclick="window.fieldcraftDoLogin()">Continue</button>
      </div>
    </div>`;
    document.querySelector('#login-token').addEventListener('keydown', (event) => {
      if (event.key === 'Enter') window.fieldcraftDoLogin();
    });
  }

  function topbar() {
    return `<header class="topbar">
      <div class="topbar-brand">
        <div class="app-label">Fieldcraft</div>
        <h1>Lesson Browser</h1>
      </div>
      <div class="topbar-controls">
        <div class="project-selector">
          <label class="project-label" for="project-input">Project</label>
          <input id="project-input" class="project-input" type="text" placeholder="project-id" value="${esc(state.projectId)}" />
        </div>
        ${state.machineId ? `<span class="meta-pill">${esc(state.machineId)}</span>` : ''}
        ${state.token ? '<button class="ghost-button" onclick="window.fieldcraftLogout()">Sign out</button>' : ''}
      </div>
    </header>`;
  }

  function scoreBar(score) {
    const pct = Math.round(Math.min(1, Math.max(0, score)) * 100);
    return `<span class="score-bar" title="Score: ${pct}%"><span class="score-fill" style="width:${pct}%"></span></span>`;
  }

  function kindBadge(kind) {
    return kind ? `<span class="kind-badge kind-${esc(kind.toLowerCase())}">${esc(kind)}</span>` : '';
  }

  function renderHitList() {
    if (state.loading) {
      return `<div class="empty-card">Searching…</div>`;
    }
    if (!state.query) {
      return `<div class="empty-card">Enter a query to search lessons.</div>`;
    }
    if (!state.hits.length) {
      return `<div class="empty-card">No results for <strong>${esc(state.query)}</strong>.</div>`;
    }
    return `<div class="hit-list">
      ${state.hits.map((hit) => `
        <button class="hit-row${state.selectedId === hit.id ? ' active' : ''}" onclick="window.fieldcraftOpenNode('${esc(hit.id)}')">
          <div class="hit-row-header">
            ${kindBadge(hit.kind)}
            ${scoreBar(hit.score)}
          </div>
          <div class="hit-row-title">${esc(hit.title)}</div>
          ${hit.support_chain && hit.support_chain.length > 0
            ? `<div class="hit-row-chain">${hit.support_chain.map((item) => `<span class="chain-item">${esc(item.title)}</span>`).join(' › ')}</div>`
            : ''}
        </button>
      `).join('')}
    </div>`;
  }

  function renderNodeDetail() {
    if (!state.selected) {
      return `<section class="detail-pane empty-card">
        <div class="section-title">Select a result</div>
        <p>Choose a hit from the left pane to inspect it.</p>
      </section>`;
    }

    const { node, relations, project_id } = state.selected;
    const nodeData = node || {};
    const relList = Array.isArray(relations) ? relations : [];

    const bodyHtml = nodeData.body ? renderMd(nodeData.body) : '';

    const metaPills = [
      nodeData.kind,
      nodeData.machine,
      nodeData.source_kind,
      project_id,
    ].filter(Boolean).map((v) => `<span class="meta-pill">${esc(v)}</span>`).join('');

    const relHtml = relList.length > 0
      ? `<section class="rel-section">
          <div class="section-title">Relations</div>
          <ul class="rel-list">
            ${relList.map((rel) => `<li class="rel-item">
              <span class="rel-predicate">${esc(rel.predicate || rel.relation_type || '')}</span>
              <button class="rel-target ghost-button" onclick="window.fieldcraftOpenNode('${esc(rel.target_node_id || rel.object_id || '')}')">
                ${esc(rel.target_title || rel.target_node_id || rel.object_id || '')}
              </button>
            </li>`).join('')}
          </ul>
        </section>`
      : '';

    return `<section class="detail-pane">
      <div class="detail-header">
        <div>
          <div class="section-title">Node</div>
          <h2>${esc(nodeData.title || state.selectedId || '')}</h2>
          <div class="detail-meta">${metaPills}</div>
        </div>
        <div class="detail-id">
          <span class="node-id">${esc(nodeData.node_id || state.selectedId || '')}</span>
        </div>
      </div>
      ${bodyHtml ? `<div class="note-content">${bodyHtml}</div>` : ''}
      ${relHtml}
    </section>`;
  }

  function renderAppShell() {
    app.innerHTML = `${topbar()}
      <main class="app-shell">
        <section class="primary-column">
          <div class="primary-toolbar">
            <input id="search-input" class="search-input" placeholder="Search lessons…" value="${esc(state.searchDraft)}" />
          </div>
          <div class="primary-scroll">
            ${renderHitList()}
          </div>
        </section>
        ${renderNodeDetail()}
      </main>`;

    const searchInput = document.querySelector('#search-input');
    if (searchInput) {
      searchInput.addEventListener('input', (event) => {
        state.searchDraft = event.target.value;
        debounceSearch();
      });
      searchInput.addEventListener('keydown', (event) => {
        if (event.key === 'Enter') {
          clearTimeout(debounceTimer);
          applySearch();
        }
      });
    }

    const projectInput = document.querySelector('#project-input');
    if (projectInput) {
      projectInput.addEventListener('change', (event) => {
        state.projectId = event.target.value.trim();
        localStorage.setItem('fieldcraft_project', state.projectId);
        if (state.query) applySearch();
      });
    }
  }

  async function loadStatus() {
    const envelope = await api('GET', '/api/v1/status');
    const data = envelope.data || envelope;
    state.machineId = data.machine_id || '';
    state.authRequired = Boolean(data.auth_required);
  }

  async function searchLessons() {
    if (!state.projectId) {
      state.hits = [];
      return;
    }
    state.loading = true;
    renderAppShell();
    const response = await api(
      'POST',
      `/api/v1/projects/${encodeURIComponent(state.projectId)}/query`,
      { project_id: state.projectId, query: state.query, mode: 'default' }
    );
    state.hits = response.hits || [];
    state.loading = false;
  }

  async function loadNode(nodeId) {
    if (!nodeId || !state.projectId) return;
    const envelope = await api(
      'GET',
      `/api/v1/projects/${encodeURIComponent(state.projectId)}/nodes/${encodeURIComponent(nodeId)}`
    );
    state.selected = envelope.data || envelope;
  }

  let debounceTimer = null;
  function applySearch() {
    const next = state.searchDraft.trim();
    if (!next) {
      state.query = '';
      state.hits = [];
      state.selected = null;
      state.selectedId = null;
      renderAppShell();
      return;
    }
    state.query = next;
    state.selected = null;
    state.selectedId = null;
    searchLessons().then(renderAppShell).catch(handleError);
  }

  function debounceSearch() {
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(applySearch, 300);
  }

  function handleError(error) {
    if (error.status === 401) {
      renderLogin('Invalid token');
      return;
    }
    app.innerHTML = `${topbar()}<div class="error-banner standalone">${esc(error.message || 'Something went wrong')}</div>`;
  }

  window.fieldcraftOpenNode = function (nodeId) {
    if (!nodeId) return;
    state.selectedId = nodeId;
    loadNode(nodeId).then(renderAppShell).catch(handleError);
  };

  window.fieldcraftLogout = function () {
    state.token = '';
    localStorage.removeItem('fieldcraft_token');
    state.machineId = '';
    renderLogin();
  };

  window.fieldcraftDoLogin = async function () {
    state.token = document.querySelector('#login-token').value.trim();
    localStorage.setItem('fieldcraft_token', state.token);
    try {
      await boot();
    } catch (error) {
      state.token = '';
      localStorage.removeItem('fieldcraft_token');
      renderLogin(error.status === 401 ? 'Invalid token' : error.message);
    }
  };

  async function boot() {
    await loadStatus();
    if (state.authRequired && !state.token) {
      renderLogin();
      return;
    }
    state.searchDraft = state.query;
    renderAppShell();
  }

  boot().catch(handleError);
})();
