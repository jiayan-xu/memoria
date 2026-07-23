// Memoria Dashboard client (P2-5) — authz headers + danger confirm
const AUTH_KEYS = { id: 'memoria_agent_id', key: 'memoria_agent_key', ns: 'memoria_namespace' };
// 主数据命名空间（~98% 记忆在此）；旧默认 default 几乎无业务边
const DEFAULT_NS = 'agent/xujiayan';
const _rawEndpoints = ['/stats', '/graph', '/decay_timeline'];

function resolveNs(raw) {
  const v = (raw || '').trim();
  if (!v || v === 'default') return DEFAULT_NS;
  return v;
}

function loadAuthIntoForm() {
  const idEl = document.getElementById('authAgentId');
  if (!idEl) return;
  idEl.value = localStorage.getItem(AUTH_KEYS.id) || '';
  document.getElementById('authAgentKey').value = localStorage.getItem(AUTH_KEYS.key) || '';
  const ns = resolveNs(localStorage.getItem(AUTH_KEYS.ns));
  document.getElementById('authNamespace').value = ns;
  // 迁移：localStorage 仍是 default/空时写回主 ns，避免每次刷新又落回旧默认
  if (resolveNs(localStorage.getItem(AUTH_KEYS.ns)) === DEFAULT_NS) {
    localStorage.setItem(AUTH_KEYS.ns, DEFAULT_NS);
  }
  const lab = document.getElementById('settingsNsLabel');
  if (lab) lab.textContent = currentNs();
}

function saveAuth() {
  localStorage.setItem(AUTH_KEYS.id, document.getElementById('authAgentId').value.trim());
  localStorage.setItem(AUTH_KEYS.key, document.getElementById('authAgentKey').value.trim());
  localStorage.setItem(AUTH_KEYS.ns, resolveNs(document.getElementById('authNamespace').value));
  document.getElementById('authNamespace').value = currentNs();
  const lab = document.getElementById('settingsNsLabel');
  if (lab) lab.textContent = currentNs();
  showToast('凭证已保存到本机', 'success');
  loadStats();
}

function currentNs() {
  return resolveNs(
    document.getElementById('authNamespace')?.value || localStorage.getItem(AUTH_KEYS.ns) || DEFAULT_NS
  );
}

function authHeaders(extra) {
  const id = (document.getElementById('authAgentId')?.value || localStorage.getItem(AUTH_KEYS.id) || '').trim();
  const key = (document.getElementById('authAgentKey')?.value || localStorage.getItem(AUTH_KEYS.key) || '').trim();
  if (!id || !key) throw new Error('请先在顶栏填写并保存 Agent Id / Key');
  return Object.assign({ 'Content-Type': 'application/json', 'X-Agent-Id': id, 'X-Agent-Key': key }, extra || {});
}

function confirmDanger(actionLabel) {
  const typed = prompt('危险操作：' + actionLabel + '\n\n请输入大写 CONFIRM 继续：');
  return typed === 'CONFIRM';
}

async function api(path, opts) {
  opts = opts || {};
  const prefix = _rawEndpoints.some(function (p) { return path.startsWith(p); }) ? '' : '/api';
  const headers = authHeaders(opts.headers || {});
  const res = await fetch(prefix + path, Object.assign({}, opts, { headers: headers }));
  if (!res.ok) {
    const err = await res.json().catch(function () { return { detail: res.statusText }; });
    const msg = err.detail || err.message || res.statusText;
    if (res.status === 401) throw new Error('未授权 (401)：检查 Agent Id/Key');
    if (res.status === 403) throw new Error('禁止访问 (403)：命名空间无权限');
    if (res.status === 428) throw new Error('需要二次确认头 X-Confirm');
    throw new Error(msg);
  }
  const ct = res.headers.get('content-type') || '';
  if (ct.indexOf('application/zip') >= 0 || opts.raw) return res;
  return res.json();
}

async function mcpCall(name, args) {
  args = args || {};
  const body = {
    jsonrpc: '2.0', id: Date.now(), method: 'tools/call',
    params: { name: name, arguments: Object.assign({ namespace: currentNs() }, args) }
  };
  const res = await fetch('/mcp', { method: 'POST', headers: authHeaders(), body: JSON.stringify(body) });
  const data = await res.json();
  if (data.error) throw new Error(data.error.message || JSON.stringify(data.error));
  const text = data.result && data.result.content && data.result.content[0] && data.result.content[0].text;
  if (!text) return data.result;
  try { return JSON.parse(text); } catch (e) { return { raw: text }; }
}

document.querySelectorAll('nav button').forEach(function (btn) {
  btn.addEventListener('click', function () {
    document.querySelectorAll('nav button').forEach(function (b) { b.classList.remove('active'); });
    document.querySelectorAll('.tab').forEach(function (t) { t.classList.remove('active'); });
    btn.classList.add('active');
    document.getElementById('tab-' + btn.dataset.tab).classList.add('active');
    if (btn.dataset.tab === 'graph') loadGraph();
    if (btn.dataset.tab === 'settings') {
      var lab = document.getElementById('settingsNsLabel');
      if (lab) lab.textContent = currentNs();
    }
    if (btn.dataset.tab === 'browse') loadMemories();
  });
});

async function loadStats() {
  try {
    const s = await api('/stats?namespace=' + encodeURIComponent(currentNs()));
    document.getElementById('statHot').textContent = s.memories.hot;
    document.getElementById('statWarm').textContent = s.memories.warm;
    document.getElementById('statCold').textContent = s.memories.cold;
    document.getElementById('statTotal').textContent = s.memories.hot + s.memories.warm + s.memories.cold;
  } catch (e) {
    document.getElementById('statTotal').textContent = '!';
  }
}

async function search() {
  const q = document.getElementById('searchInput').value.trim();
  if (!q) return;
  const container = document.getElementById('searchResults');
  container.innerHTML = '<div class="empty"><div class="icon">🔎</div>搜索中...</div>';
  try {
    const parsed = await mcpCall('memory_search_v2', { query: q, max_results: 10 });
    if (!parsed.results || !parsed.results.length) {
      container.innerHTML = '<div class="empty"><div class="icon">📭</div>未找到相关记忆</div>';
      return;
    }
    renderMemoryCards(container, parsed.results);
  } catch (e) {
    container.innerHTML = '<div class="empty"><div class="icon">⚠️</div>' + escHtml(e.message) + '</div>';
  }
}

var currentPage = 1;
async function loadMemories(page) {
  page = page || 1;
  currentPage = page;
  const tier = document.getElementById('filterTier').value;
  const category = document.getElementById('filterCategory').value;
  const searchQ = document.getElementById('filterSearch').value.trim();
  const params = new URLSearchParams({ page: page, limit: 30, namespace: currentNs() });
  if (tier) params.set('tier', tier);
  if (category) params.set('category', category);
  if (searchQ) params.set('search', searchQ);
  const container = document.getElementById('browseResults');
  container.innerHTML = '<div class="empty"><div class="icon">📂</div>加载中...</div>';
  try {
    const data = await api('/memories?' + params);
    container.innerHTML = '';
    if (!data.memories.length) {
      container.innerHTML = '<div class="empty"><div class="icon">📭</div>暂无记忆</div>';
      return;
    }
    data.memories.forEach(function (m) { container.appendChild(createMemoryCard(m)); });
    const totalPages = Math.ceil((data.total || data.memories.length) / (data.limit || 30));
    const pg = document.getElementById('pagination');
    pg.innerHTML = '';
    if (totalPages > 1) {
      for (var i = 1; i <= Math.min(totalPages, 20); i++) {
        (function (n) {
          var btn = document.createElement('button');
          btn.textContent = n;
          if (n === page) btn.classList.add('active');
          btn.onclick = function () { loadMemories(n); };
          pg.appendChild(btn);
        })(i);
      }
    }
  } catch (e) {
    container.innerHTML = '<div class="empty"><div class="icon">⚠️</div>' + escHtml(e.message) + '</div>';
  }
}

function createMemoryCard(m) {
  const card = document.createElement('div');
  card.className = 'memory-card';
  const mid = m.id || m.memory_id;
  card.onclick = function () { openDetail(mid); };
  const tier = m.tier || 'warm';
  const tierLabel = { hot: '🔥 HOT', warm: '🌡️ WARM', cold: '❄️ COLD' }[tier] || tier;
  const category = m.category || 'fact';
  const imp = m.importance || 3;
  const decay = m.decay_factor != null ? (m.decay_factor * 100).toFixed(0) + '%' : '—';
  var content = m.content || '';
  var source = m.source || '';
  card.innerHTML =
    '<div class="meta">' +
    '<span class="badge badge-' + tier + '">' + tierLabel + '</span>' +
    '<span class="badge badge-importance">⭐' + imp + '</span>' +
    '<span style="color:var(--text2)">📉 ' + decay + '</span>' +
    '<span style="color:var(--text2);margin-left:auto">' + escHtml(source) + '</span>' +
    '<span style="color:var(--text2)">' + escHtml(category) + '</span></div>' +
    '<div class="content-preview">' + escHtml(content) + '</div>';
  return card;
}

function renderMemoryCards(container, items) {
  container.innerHTML = '';
  items.forEach(function (m) { container.appendChild(createMemoryCard(m)); });
}

async function openDetail(id) {
  if (!id) return;
  const overlay = document.getElementById('modalOverlay');
  const content = document.getElementById('modalContent');
  content.innerHTML = '<p style="color:var(--text2)">加载中...</p>';
  overlay.classList.add('show');
  try {
    const m = await api('/memories/' + id);
    var cats = ['decision', 'preference', 'constraint', 'lesson', 'fact', 'conversation'];
    content.innerHTML =
      '<button class="close-btn" onclick="closeModal()" style="position:static;float:right;background:none;border:none;color:var(--text2);font-size:1.3rem;cursor:pointer">✕</button>' +
      '<h3>' + escHtml((m.content || '').substring(0, 60)) + '...</h3>' +
      '<div class="field"><label>全文</label><textarea id="editContent">' + escHtml(m.content || '') + '</textarea></div>' +
      '<div class="field" style="display:flex;gap:8px">' +
      '<div style="flex:1"><label>分类</label><select id="editCategory">' +
      cats.map(function (c) { return '<option value="' + c + '"' + (m.category === c ? ' selected' : '') + '>' + c + '</option>'; }).join('') +
      '</select></div>' +
      '<div style="flex:1"><label>层级</label><select id="editTier">' +
      ['hot', 'warm', 'cold'].map(function (t) { return '<option value="' + t + '"' + (m.tier === t ? ' selected' : '') + '>' + t + '</option>'; }).join('') +
      '</select></div>' +
      '<div style="flex:1"><label>重要性</label><select id="editImportance">' +
      [1, 2, 3, 4, 5].map(function (i) { return '<option value="' + i + '"' + (m.importance === i ? ' selected' : '') + '>⭐' + i + '</option>'; }).join('') +
      '</select></div></div>' +
      '<div class="field"><label>ID: ' + escHtml(id) + ' · NS: ' + escHtml(m.namespace || '') + '</label></div>' +
      '<div class="actions">' +
      '<button class="btn-danger" onclick="deleteMemory(\'' + escHtml(id) + '\')">删除</button>' +
      '<button class="btn-ghost" onclick="closeModal()">取消</button>' +
      '<button class="btn-save" onclick="saveMemory(\'' + escHtml(id) + '\')">保存</button></div>';
  } catch (e) {
    content.innerHTML = '<p style="color:var(--danger)">错误: ' + escHtml(e.message) + '</p>';
  }
}

function closeModal() {
  document.getElementById('modalOverlay').classList.remove('show');
}

async function saveMemory(id) {
  try {
    await api('/memories/' + id, {
      method: 'PUT',
      body: JSON.stringify({
        content: document.getElementById('editContent').value,
        category: document.getElementById('editCategory').value,
        tier: document.getElementById('editTier').value,
        importance: parseInt(document.getElementById('editImportance').value, 10)
      })
    });
    closeModal();
    showToast('已保存', 'success');
    loadStats();
    if (document.getElementById('tab-browse').classList.contains('active')) loadMemories(currentPage);
  } catch (e) { showToast(e.message, 'error'); }
}

async function deleteMemory(id) {
  if (!confirmDanger('删除记忆 ' + id)) return;
  try {
    await api('/memories/' + id, { method: 'DELETE', headers: { 'X-Confirm': 'delete-memory' } });
    closeModal();
    showToast('已删除', 'success');
    loadStats();
    if (document.getElementById('tab-browse').classList.contains('active')) loadMemories(currentPage);
  } catch (e) { showToast(e.message, 'error'); }
}

var graphNetwork = null;
async function loadGraph() {
  try {
    const data = await api('/graph?namespace=' + encodeURIComponent(currentNs()) + '&limit=1200');
    const nodes = (data.nodes || []).map(function (n) {
      const tier = n.tier || n.group || 'warm';
      return {
        id: n.id,
        label: (n.label || n.content || '').substring(0, 30),
        title: '<b>' + (n.category || '?') + '</b> · ' + tier + '<br>' + (n.title || n.content || '').substring(0, 200),
        color: {
          background: n.color || (tier === 'hot' ? '#ff6b6b' : tier === 'cold' ? '#6c8ebf' : '#ffd93d'),
          border: '#1a1a23'
        },
        size: n.size || (10 + (n.importance || 3) * 4),
        font: { size: 12, color: '#e4e4f0' },
        borderWidth: 2
      };
    });
    const edges = (data.edges || []).map(function (e) {
      const w = Number(e.weight != null ? e.weight : (e.value != null ? e.value : 0.5));
      const ww = isFinite(w) ? Math.max(0.05, Math.min(1, w)) : 0.5;
      return {
        from: e.source || e.from,
        to: e.target || e.to,
        title: e.title || e.relation_type || '',
        color: {
          color: '#8b9cb3',
          highlight: '#c5d0e0',
          hover: '#a8b8cc',
          opacity: 0.55 + ww * 0.45
        },
        width: Math.max(1.5, ww * 4),
        smooth: { type: 'continuous', roundness: 0.2 }
      };
    });
    const container = document.getElementById('graph-container');
    if (graphNetwork) graphNetwork.destroy();
    const summary = data.summary || {};
    graphNetwork = new vis.Network(container, { nodes: new vis.DataSet(nodes), edges: new vis.DataSet(edges) }, {
      physics: { solver: 'forceAtlas2Based', forceAtlas2Based: { gravitationalConstant: -30, centralGravity: 0.005 } },
      interaction: { hover: true, tooltipDelay: 200 },
      nodes: { shape: 'dot' },
      edges: { selectionWidth: 3 }
    });
    graphNetwork.on('click', function (params) {
      if (params.nodes.length) openDetail(params.nodes[0]);
    });
    const hint = document.getElementById('graph-hint');
    if (hint) {
      var tm = summary.total_memories != null ? (' / ' + summary.total_memories) : '';
      var tr = summary.total_relations != null ? (' / ' + summary.total_relations) : '';
      hint.textContent = 'NS ' + currentNs() + ' · 点 ' + nodes.length + tm + ' · 边 ' + edges.length + tr + ' (采样预览)';
    }
  } catch (e) {
    document.getElementById('graph-container').innerHTML = '<div class="empty"><div class="icon">⚠️</div>' + escHtml(e.message) + '</div>';
  }
}

async function exportData() {
  if (!confirmDanger('导出命名空间 ' + currentNs())) return;
  try {
    const parsed = await mcpCall('memory_export', { include_vectors: false });
    const text = typeof parsed.export === 'string' ? parsed.export : JSON.stringify(parsed, null, 2);
    const blob = new Blob([text], { type: 'application/x-ndjson' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = 'memoria_' + currentNs() + '_' + new Date().toISOString().slice(0, 10) + '.jsonl';
    a.click();
    URL.revokeObjectURL(url);
    showToast('导出完成', 'success');
  } catch (e) { showToast(e.message, 'error'); }
}

async function importData() {
  const jsonl = document.getElementById('importJsonl').value.trim();
  if (!jsonl) { showToast('请先粘贴 JSONL', 'error'); return; }
  if (!confirmDanger('导入到命名空间 ' + currentNs() + '（不可轻易撤销）')) return;
  try {
    const parsed = await mcpCall('memory_import', { jsonl: jsonl });
    showToast((parsed.status === 'ok' || parsed.status === 'imported') ? '导入完成' : JSON.stringify(parsed), 'success');
    loadStats();
  } catch (e) { showToast(e.message, 'error'); }
}

async function triggerDecay() {
  if (!confirmDanger('运行记忆衰减')) return;
  try {
    await mcpCall('memory_decay', { action: 'run' });
    showToast('衰减已完成', 'success');
    loadStats();
  } catch (e) { showToast(e.message, 'error'); }
}

async function triggerBackup() {
  if (!confirmDanger('触发 GFS 备份（需 admin）')) return;
  try {
    const r = await mcpCall('memory_backup', {});
    showToast((r.status === 'ok' || r.backup_path) ? '备份已触发' : JSON.stringify(r), 'success');
  } catch (e) { showToast(e.message, 'error'); }
}

function showToast(msg, type) {
  type = type || 'success';
  const t = document.createElement('div');
  t.className = 'toast toast-' + type;
  t.textContent = msg;
  document.getElementById('toastContainer').appendChild(t);
  setTimeout(function () { t.remove(); }, 3000);
}

function escHtml(s) {
  if (!s) return '';
  const d = document.createElement('div');
  d.textContent = s;
  return d.innerHTML;
}

loadAuthIntoForm();
loadStats();
