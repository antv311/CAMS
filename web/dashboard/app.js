(() => {
  'use strict';

  const TOKEN_STORAGE_KEY = 'aircd_dashboard_token';
  const VIEW_STORAGE_KEY = 'aircd_dashboard_view';

  const state = {
    token: localStorage.getItem(TOKEN_STORAGE_KEY) || '',
    viewMode: localStorage.getItem(VIEW_STORAGE_KEY) || 'board',
    project: '',
    sprint: '',
    search: '',
    statusGroup: 'all',
    assigneeFilter: '',
    includeTerminal: true,
    infraTab: 'repos',
    selectedChannel: '',
    channelMessages: null,
    refreshMs: 5000,
    timerId: null,
    snapshot: null,
    publicSnapshot: null,
    authFailed: false,
  };

  const els = {
    connBadge: document.getElementById('connBadge'),
    tokenInput: document.getElementById('tokenInput'),
    toggleTokenBtn: document.getElementById('toggleTokenBtn'),
    connectBtn: document.getElementById('connectBtn'),
    disconnectBtn: document.getElementById('disconnectBtn'),
    projectFilter: document.getElementById('projectFilter'),
    sprintFilter: document.getElementById('sprintFilter'),
    searchInput: document.getElementById('searchInput'),
    includeTerminal: document.getElementById('includeTerminal'),
    viewBoardBtn: document.getElementById('viewBoardBtn'),
    viewTableBtn: document.getElementById('viewTableBtn'),
    refreshInterval: document.getElementById('refreshInterval'),
    refreshBtn: document.getElementById('refreshBtn'),
    lastUpdated: document.getElementById('lastUpdated'),
    banner: document.getElementById('banner'),
    kpiGrid: document.getElementById('kpiGrid'),
    sprintsSection: document.getElementById('sprintsSection'),
    sprintMeta: document.getElementById('sprintMeta'),
    sprintsGrid: document.getElementById('sprintsGrid'),
    tasksSection: document.getElementById('tasksSection'),
    tasksMeta: document.getElementById('tasksMeta'),
    statusPills: document.getElementById('statusPills'),
    tasksContent: document.getElementById('tasksContent'),
    agentsSection: document.getElementById('agentsSection'),
    agentsMeta: document.getElementById('agentsMeta'),
    agentFilterBadge: document.getElementById('agentFilterBadge'),
    agentsContent: document.getElementById('agentsContent'),
    infraMeta: document.getElementById('infraMeta'),
    infraContent: document.getElementById('infraContent'),
    taskModal: document.getElementById('taskModal'),
    modalTitle: document.getElementById('modalTitle'),
    modalSubtitle: document.getElementById('modalSubtitle'),
    modalBody: document.getElementById('modalBody'),
    closeModalBtn: document.getElementById('closeModalBtn'),
  };

  function esc(val) {
    if (val === null || val === undefined) return '';
    return String(val)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;')
      .replace(/'/g, '&#39;');
  }

  function safeHttpUrl(raw) {
    if (!raw) return null;
    try {
      const u = new URL(String(raw));
      if (u.protocol === 'http:' || u.protocol === 'https:') return u.href;
    } catch (_) {
      return null;
    }
    return null;
  }

  function fmtTime(unixSec) {
    if (!unixSec) return '—';
    const d = new Date(unixSec * 1000);
    return d.toLocaleString();
  }

  function fmtRelative(unixSec) {
    if (!unixSec) return '—';
    const diff = Math.max(0, Math.floor(Date.now() / 1000) - unixSec);
    if (diff < 60) return `${diff}s ago`;
    if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
    if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
    return `${Math.floor(diff / 86400)}d ago`;
  }

  function fmtCountdown(untilSec) {
    const left = Math.max(0, untilSec - Math.floor(Date.now() / 1000));
    if (left < 60) return `${left}s`;
    return `${Math.ceil(left / 60)}m`;
  }

  function shortId(id) {
    if (!id) return '';
    return id.length > 14 ? `${id.slice(0, 12)}…` : id;
  }

  function statusBadgeClass(status) {
    switch (status) {
      case 'merged':
      case 'done':
        return 'badge-ok';
      case 'in_progress':
      case 'testing':
      case 'claimed':
        return 'badge-accent';
      case 'review':
      case 'super_review':
        return 'badge-purple';
      case 'blocked':
      case 'changes_requested':
        return 'badge-danger';
      case 'assigned':
      case 'open':
        return 'badge-warn';
      default:
        return 'badge-muted';
    }
  }

  function kindBadgeClass(kind) {
    if (kind === 'bug') return 'badge-danger';
    if (kind === 'feature') return 'badge-accent';
    return 'badge-muted';
  }

  function statusGroupOf(status) {
    if (status === 'open' || status === 'assigned') return 'backlog';
    if (status === 'claimed' || status === 'in_progress' || status === 'testing') return 'active';
    if (status === 'blocked') return 'blocked';
    if (status === 'review' || status === 'super_review' || status === 'changes_requested') return 'review';
    return 'completed';
  }

  function setBanner(kind, html) {
    if (!html) {
      els.banner.hidden = true;
      els.banner.className = 'banner';
      els.banner.innerHTML = '';
      return;
    }
    els.banner.hidden = false;
    els.banner.className = `banner ${kind || ''}`.trim();
    els.banner.innerHTML = html;
  }

  async function apiFetch(path, withAuth = true) {
    const headers = {};
    if (withAuth && state.token) {
      headers['Authorization'] = `Bearer ${state.token}`;
    }
    const res = await fetch(path, { headers });
    let data = null;
    try {
      data = await res.json();
    } catch (_) {
      data = null;
    }
    if (!res.ok) {
      const err = new Error((data && data.error) || `HTTP ${res.status}`);
      err.status = res.status;
      err.data = data;
      throw err;
    }
    return data;
  }

  async function loadData() {
    if (state.token && !state.authFailed) {
      try {
        const params = new URLSearchParams();
        if (state.project) params.set('channel', state.project);
        if (state.sprint) params.set('sprint', state.sprint);
        params.set('include_terminal', state.includeTerminal ? 'true' : 'false');
        const query = params.toString();
        const snap = await apiFetch(`/v1/dashboard?${query}`, true);
        state.snapshot = snap;
        state.publicSnapshot = null;
        els.connBadge.className = 'badge badge-ok';
        els.connBadge.textContent = `Connected @${snap.viewer.nick} (${snap.viewer.role})`;
        setBanner('', '');
        renderAll();
        els.lastUpdated.textContent = `Updated ${new Date().toLocaleTimeString()}`;
        return;
      } catch (err) {
        if (err.status === 401 || err.status === 403) {
          state.authFailed = true;
          els.connBadge.className = 'badge badge-danger';
          els.connBadge.textContent = err.status === 401 ? 'Token rejected (401)' : 'Scope denied (403)';
          setBanner(
            'err',
            `<span><strong>Authentication error:</strong> ${esc(err.message)}. Update your Read-scoped Bearer token and click Connect.</span>`
          );
        } else if (err.status === 429) {
          const waitSec = err.data && err.data.retry_after_seconds ? err.data.retry_after_seconds : 5;
          setBanner('warn', `<span><strong>Rate limited:</strong> retry after ${esc(waitSec)}s.</span>`);
          return;
        } else {
          setBanner('err', `<span><strong>Failed to load dashboard:</strong> ${esc(err.message)}</span>`);
          return;
        }
      }
    }

    // Public preview mode (no token, or auth failed)
    try {
      const [health, agentsRes, channelsRes, gitsRes, locksRes] = await Promise.all([
        apiFetch('/healthz', false),
        apiFetch('/v1/agents', false),
        apiFetch('/v1/channels', false),
        apiFetch('/v1/gits', false),
        apiFetch('/v1/locks', false),
      ]);
      state.snapshot = null;
      state.publicSnapshot = {
        health,
        agents: agentsRes.agents || [],
        channels: channelsRes.channels || [],
        git_repos: gitsRes.git_repos || [],
        locks: locksRes.locks || [],
      };
      if (!state.authFailed) {
        els.connBadge.className = 'badge badge-warn';
        els.connBadge.textContent = 'Public Preview';
        setBanner(
          'warn',
          '<span><strong>Public preview mode:</strong> showing public agents, channels, repos, and locks. Enter a Bearer token (<code>Read</code> scope) above to monitor projects, sprint rooms, tasks, and agent wake status.</span>'
        );
      }
      renderAll();
      els.lastUpdated.textContent = `Updated ${new Date().toLocaleTimeString()}`;
    } catch (err) {
      els.connBadge.className = 'badge badge-danger';
      els.connBadge.textContent = 'Offline';
      setBanner('err', `<span><strong>Hub unreachable:</strong> ${esc(err.message)}</span>`);
    }
  }

  function syncFilterOptions() {
    const snap = state.snapshot;
    if (!snap) {
      els.projectFilter.innerHTML = '<option value="">All Projects</option>';
      els.sprintFilter.innerHTML = '<option value="">All Sprints</option>';
      return;
    }

    const prevProj = state.project;
    const projOpts = ['<option value="">All Projects</option>']
      .concat(
        (snap.projects || []).map(
          (p) => `<option value="${esc(p.channel)}">${esc(p.name)} (${esc(p.channel)})</option>`
        )
      )
      .join('');
    els.projectFilter.innerHTML = projOpts;
    els.projectFilter.value = prevProj;
    if (els.projectFilter.value !== prevProj) state.project = '';

    const prevSprint = state.sprint;
    const rooms = (snap.rooms || []).filter((r) => !state.project || r.project === state.project);
    const sprintOpts = ['<option value="">All Sprints</option>']
      .concat(
        rooms.map((r) => {
          const label = r.title ? `${r.channel} — ${r.title}` : r.channel;
          return `<option value="${esc(r.channel)}">${esc(label)}</option>`;
        })
      )
      .join('');
    els.sprintFilter.innerHTML = sprintOpts;
    els.sprintFilter.value = prevSprint;
    if (els.sprintFilter.value !== prevSprint) state.sprint = '';
  }

  function renderKpis() {
    if (!state.snapshot) {
      const pub = state.publicSnapshot;
      if (!pub) {
        els.kpiGrid.innerHTML = '';
        return;
      }
      const online = pub.agents.filter((a) => a.online).length;
      els.kpiGrid.innerHTML = `
        <div class="kpi-card">
          <div class="kpi-label">Hub Status</div>
          <div class="kpi-value" style="color:var(--ok)">Healthy</div>
          <div class="kpi-sub">HTTP :${esc(pub.health.http_port)} &middot; IRC :${esc(pub.health.irc_port)}</div>
        </div>
        <div class="kpi-card">
          <div class="kpi-label">Agents Online</div>
          <div class="kpi-value">${online} / ${pub.agents.length}</div>
          <div class="kpi-sub">Registered accounts</div>
        </div>
        <div class="kpi-card">
          <div class="kpi-label">Git Repos</div>
          <div class="kpi-value">${pub.git_repos.length}</div>
          <div class="kpi-sub">Connected workspaces</div>
        </div>
        <div class="kpi-card">
          <div class="kpi-label">Active Locks</div>
          <div class="kpi-value">${pub.locks.length}</div>
          <div class="kpi-sub">Equipment &amp; branch locks</div>
        </div>
      `;
      return;
    }

    const s = state.snapshot.summary;
    const tc = s.filtered_task_counts;
    const denom = Math.max(0, tc.total - tc.cancelled);
    const pct = denom > 0 ? Math.round((tc.completed / denom) * 100) : 0;
    const inFlight = tc.in_progress + tc.testing + tc.claimed;
    const inReview = tc.review + tc.super_review + tc.changes_requested;

    els.kpiGrid.innerHTML = `
      <div class="kpi-card">
        <div class="kpi-label">Sprint Completion</div>
        <div class="kpi-value">${pct}%</div>
        <div class="progress-track"><div class="progress-fill" style="width:${pct}%"></div></div>
        <div class="kpi-sub">${tc.completed} of ${denom} done/merged${tc.cancelled ? ` (${tc.cancelled} cancelled)` : ''}</div>
      </div>
      <div class="kpi-card">
        <div class="kpi-label">In Flight</div>
        <div class="kpi-value" style="color:var(--accent)">${inFlight}</div>
        <div class="kpi-sub">${tc.in_progress} in progress &middot; ${tc.testing} testing &middot; ${tc.assigned + tc.open} queued</div>
      </div>
      <div class="kpi-card">
        <div class="kpi-label">Review Queue</div>
        <div class="kpi-value" style="color:var(--purple)">${inReview}</div>
        <div class="kpi-sub">${tc.review} review &middot; ${tc.super_review} super &middot; ${tc.changes_requested} changes req</div>
      </div>
      <div class="kpi-card">
        <div class="kpi-label">Blocked / Questions</div>
        <div class="kpi-value" style="color:${tc.blocked > 0 ? 'var(--danger)' : 'var(--ok)'}">${tc.blocked}</div>
        <div class="kpi-sub">${s.pending_questions_total} unanswered question(s)</div>
      </div>
      <div class="kpi-card">
        <div class="kpi-label">Agents &amp; Wakes</div>
        <div class="kpi-value">${s.agents_online} / ${s.agents_total}</div>
        <div class="kpi-sub">${s.wake_running_sessions} running &middot; ${s.wake_pending_sessions} queued &middot; ${s.wake_quota_cooldowns_active} cooldown</div>
      </div>
      <div class="kpi-card">
        <div class="kpi-label">Hub Activity</div>
        <div class="kpi-value">${s.irc_sessions} <span class="small muted">IRC</span></div>
        <div class="kpi-sub">${s.messages_total} msgs &middot; ${s.locks_total} locks &middot; ${s.git_repos_total} repos</div>
      </div>
    `;
  }

  function renderSprints() {
    const snap = state.snapshot;
    if (!snap) {
      els.sprintsSection.hidden = true;
      return;
    }
    const rooms = (snap.rooms || []).filter((r) => !state.project || r.project === state.project);
    els.sprintsSection.hidden = false;
    els.sprintMeta.textContent = `${rooms.length} sprint room(s)`;

    if (rooms.length === 0) {
      els.sprintsGrid.innerHTML = '<div class="muted">No sprint rooms opened yet.</div>';
      return;
    }

    els.sprintsGrid.innerHTML = rooms
      .map((r) => {
        const c = r.task_counts;
        const denom = Math.max(0, c.total - c.cancelled);
        const pct = denom > 0 ? Math.round((c.completed / denom) * 100) : 0;
        const selected = state.sprint === r.channel ? ' selected' : '';
        return `
          <div class="sprint-card${selected}" data-sprint="${esc(r.channel)}" data-project="${esc(r.project)}">
            <div class="sprint-top">
              <div>
                <span class="badge badge-accent mono">${esc(r.label)}</span>
                <strong class="mono">${esc(r.channel)}</strong>
              </div>
              <span class="small muted">${pct}% (${c.completed}/${denom})</span>
            </div>
            ${r.title ? `<div class="small">${esc(r.title)}</div>` : ''}
            <div class="progress-track"><div class="progress-fill" style="width:${pct}%"></div></div>
            <div class="sprint-counts">
              <span>${c.open + c.assigned} open/assigned</span>
              <span>${c.in_progress + c.testing + c.claimed} active</span>
              <span>${c.review + c.super_review + c.changes_requested} review</span>
              ${c.blocked ? `<span style="color:var(--danger)">${c.blocked} blocked</span>` : ''}
              <span style="color:var(--ok)">${c.completed} merged/done</span>
            </div>
          </div>
        `;
      })
      .join('');

    els.sprintsGrid.querySelectorAll('.sprint-card').forEach((card) => {
      card.addEventListener('click', () => {
        const sp = card.getAttribute('data-sprint') || '';
        const pr = card.getAttribute('data-project') || '';
        if (state.sprint === sp) {
          state.sprint = '';
        } else {
          state.sprint = sp;
          if (!state.project) state.project = pr;
        }
        loadData();
      });
    });
  }

  function filteredTasks() {
    if (!state.snapshot) return [];
    const q = state.search.trim().toLowerCase();
    return (state.snapshot.tasks || []).filter((t) => {
      if (state.statusGroup !== 'all' && statusGroupOf(t.status) !== state.statusGroup) {
        return false;
      }
      if (state.assigneeFilter) {
        const owner = (t.assignee || t.claimed_by || '').toLowerCase();
        if (owner !== state.assigneeFilter.toLowerCase()) return false;
      }
      if (!q) return true;
      const hay = [
        t.id,
        t.title,
        t.status,
        t.kind,
        t.assignee,
        t.claimed_by,
        t.reviewer,
        t.branch,
        t.repo,
        t.sprint,
        t.channel,
      ]
        .filter(Boolean)
        .join(' ')
        .toLowerCase();
      return hay.includes(q);
    });
  }

  function renderTasks() {
    if (!state.snapshot) {
      els.tasksSection.hidden = true;
      return;
    }
    els.tasksSection.hidden = false;

    const allTasks = state.snapshot.tasks || [];
    const groups = [
      { id: 'all', label: `All (${allTasks.length})` },
      { id: 'backlog', label: 'Backlog' },
      { id: 'active', label: 'In Flight' },
      { id: 'blocked', label: 'Blocked' },
      { id: 'review', label: 'Review' },
      { id: 'completed', label: 'Completed' },
    ];
    els.statusPills.innerHTML = groups
      .map(
        (g) =>
          `<button type="button" class="pill${state.statusGroup === g.id ? ' active' : ''}" data-group="${g.id}">${esc(g.label)}</button>`
      )
      .join('');
    els.statusPills.querySelectorAll('.pill').forEach((btn) => {
      btn.addEventListener('click', () => {
        state.statusGroup = btn.getAttribute('data-group') || 'all';
        renderTasks();
      });
    });

    const tasks = filteredTasks();
    const qMap = new Map();
    (state.snapshot.pending_questions || []).forEach((q) => qMap.set(q.task_id, q));
    const runningAgents = new Set(
      (state.snapshot.agents || []).filter((a) => a.wake && a.wake.running).map((a) => a.id.toLowerCase())
    );

    const filterParts = [];
    if (state.project) filterParts.push(state.project);
    if (state.sprint) filterParts.push(state.sprint);
    if (state.assigneeFilter) filterParts.push(`@${state.assigneeFilter}`);
    els.tasksMeta.textContent = `${tasks.length} shown${filterParts.length ? ' · ' + filterParts.join(' · ') : ''}`;

    if (tasks.length === 0) {
      els.tasksContent.innerHTML = '<div class="muted">No tasks match the current filters.</div>';
      return;
    }

    if (state.viewMode === 'table') {
      els.tasksContent.innerHTML = `
        <div class="table-wrap">
          <table>
            <thead>
              <tr>
                <th>Task</th>
                <th>Status</th>
                <th>Kind</th>
                <th>Sprint</th>
                <th>Assignee</th>
                <th>Reviewer</th>
                <th>Branch</th>
                <th>Updated</th>
              </tr>
            </thead>
            <tbody>
              ${tasks
                .map((t) => {
                  const owner = t.assignee || t.claimed_by;
                  const isRunning = owner && runningAgents.has(owner.toLowerCase());
                  const hasQ = qMap.has(t.id);
                  return `
                    <tr class="clickable" data-task-id="${esc(t.id)}">
                      <td>
                        <div class="task-title">${esc(t.title)}</div>
                        <div class="small mono muted">${esc(shortId(t.id))}${hasQ ? ' · <span style="color:var(--danger)">question pending</span>' : ''}</div>
                      </td>
                      <td><span class="badge ${statusBadgeClass(t.status)}">${esc(t.status)}</span></td>
                      <td><span class="badge ${kindBadgeClass(t.kind)}">${esc(t.kind)}</span></td>
                      <td class="mono small">${esc(t.sprint || t.channel)}</td>
                      <td>${owner ? `${isRunning ? '<span class="dot running" title="Agent running"></span> ' : ''}@${esc(owner)}` : '<span class="muted">—</span>'}</td>
                      <td>${t.reviewer ? `@${esc(t.reviewer)}` : '<span class="muted">—</span>'}</td>
                      <td class="mono small">${esc(t.branch || '—')}</td>
                      <td class="small muted" title="${esc(fmtTime(t.updated_at))}">${esc(fmtRelative(t.updated_at))}</td>
                    </tr>
                  `;
                })
                .join('')}
            </tbody>
          </table>
        </div>
      `;
    } else {
      const cols = [
        { id: 'backlog', title: 'Open / Assigned' },
        { id: 'active', title: 'In Progress / Testing' },
        { id: 'blocked', title: 'Blocked' },
        { id: 'review', title: 'Review / Changes Req' },
        { id: 'completed', title: 'Merged / Done' },
      ];
      els.tasksContent.innerHTML = `
        <div class="board">
          ${cols
            .map((col) => {
              const items = tasks.filter((t) => statusGroupOf(t.status) === col.id);
              return `
                <div class="board-col">
                  <div class="board-col-head">
                    <span>${esc(col.title)}</span>
                    <span>${items.length}</span>
                  </div>
                  ${items
                    .map((t) => {
                      const owner = t.assignee || t.claimed_by;
                      const isRunning = owner && runningAgents.has(owner.toLowerCase());
                      const pq = qMap.get(t.id);
                      return `
                        <div class="task-card" data-task-id="${esc(t.id)}">
                          <div class="task-card-top">
                            <span class="badge ${statusBadgeClass(t.status)}">${esc(t.status)}</span>
                            <span class="badge ${kindBadgeClass(t.kind)}">${esc(t.kind)}</span>
                          </div>
                          <div class="task-title">${esc(t.title)}</div>
                          <div class="task-meta">
                            <span class="mono">${esc(shortId(t.id))}</span>
                            ${t.sprint ? `<span class="mono">${esc(t.sprint)}</span>` : ''}
                            ${owner ? `<span>${isRunning ? '<span class="dot running" title="Agent running"></span> ' : ''}@${esc(owner)}</span>` : ''}
                          </div>
                          ${t.branch ? `<div class="small mono muted">${esc(t.branch)}</div>` : ''}
                          ${pq ? `<div class="small" style="color:var(--danger)">Q: ${esc(pq.question)}</div>` : ''}
                        </div>
                      `;
                    })
                    .join('')}
                </div>
              `;
            })
            .join('')}
        </div>
      `;
    }

    els.tasksContent.querySelectorAll('[data-task-id]').forEach((el) => {
      el.addEventListener('click', () => {
        const id = el.getAttribute('data-task-id');
        if (id) openTaskModal(id);
      });
    });
  }

  function wakeStatusBadge(agent) {
    const w = agent.wake;
    if (!w) {
      return agent.online
        ? '<span class="badge badge-ok">IRC Online</span>'
        : '<span class="badge badge-muted">Offline</span>';
    }
    if (w.running) return '<span class="badge badge-accent">Wake Running</span>';
    if (w.pending) return '<span class="badge badge-warn">Wake Queued</span>';
    if (w.quota_cooldown_until) {
      return `<span class="badge badge-danger">Cooldown (${esc(fmtCountdown(w.quota_cooldown_until))})</span>`;
    }
    if (!w.launcher_configured) return '<span class="badge badge-muted">Manual</span>';
    if (!w.launcher_enabled) return '<span class="badge badge-muted">Launcher Off</span>';
    if (!w.command_found) return '<span class="badge badge-danger">Cmd Missing</span>';
    return '<span class="badge badge-ok">Launcher Ready</span>';
  }

  function renderAgents() {
    const agents = state.snapshot
      ? state.snapshot.agents || []
      : state.publicSnapshot
        ? state.publicSnapshot.agents || []
        : [];

    const onlineCount = agents.filter((a) => a.online).length;
    els.agentsMeta.textContent = `${onlineCount}/${agents.length} online`;

    if (state.assigneeFilter) {
      els.agentFilterBadge.innerHTML = `<button type="button" class="pill active" id="clearAssigneeBtn">Filtered: @${esc(state.assigneeFilter)} &times;</button>`;
      document.getElementById('clearAssigneeBtn').addEventListener('click', () => {
        state.assigneeFilter = '';
        renderAgents();
        renderTasks();
      });
    } else {
      els.agentFilterBadge.innerHTML = '';
    }

    if (agents.length === 0) {
      els.agentsContent.innerHTML = '<div class="muted">No agents registered yet.</div>';
      return;
    }

    els.agentsContent.innerHTML = `
      <div class="agents-grid">
        ${agents
          .map((a) => {
            const w = a.wake || null;
            const dotCls = w && w.running ? 'running' : a.online ? 'online' : '';
            const isFiltered = state.assigneeFilter.toLowerCase() === a.nick.toLowerCase();
            return `
              <div class="agent-card${isFiltered ? ' active-filter' : ''}">
                <div class="agent-head">
                  <span class="agent-nick"><span class="dot ${dotCls}"></span>@${esc(a.nick)}</span>
                  ${wakeStatusBadge(a)}
                </div>
                <div class="agent-stats">
                  <span>role: <code>${esc(a.role)}</code></span>
                  ${a.team ? `<span>team: ${esc(a.team)}</span>` : ''}
                  <span>IRC: ${a.online ? 'online' : 'offline'}</span>
                  <span>hook: ${a.webhook_configured ? 'yes' : 'no'}</span>
                </div>
                ${
                  state.snapshot
                    ? `<div class="agent-stats">
                        <button type="button" class="link-btn filter-agent-btn" data-nick="${esc(a.nick)}">
                          ${a.active_task_count || 0} active task(s)
                        </button>
                        ${
                          w && w.last_run_status
                            ? `<span>last run: <strong>${esc(w.last_run_status)}</strong> (${esc(fmtRelative(w.last_run_finished_at || w.last_run_started_at))})</span>`
                            : ''
                        }
                      </div>`
                    : ''
                }
              </div>
            `;
          })
          .join('')}
      </div>
    `;

    els.agentsContent.querySelectorAll('.filter-agent-btn').forEach((btn) => {
      btn.addEventListener('click', () => {
        const nick = btn.getAttribute('data-nick') || '';
        state.assigneeFilter = state.assigneeFilter === nick ? '' : nick;
        renderAgents();
        renderTasks();
      });
    });
  }

  function renderInfra() {
    document.querySelectorAll('#infraSection [data-tab]').forEach((btn) => {
      btn.classList.toggle('active', btn.getAttribute('data-tab') === state.infraTab);
    });

    const source = state.snapshot || state.publicSnapshot;
    if (!source) {
      els.infraContent.innerHTML = '';
      return;
    }

    if (state.infraTab === 'repos') {
      const repos = source.git_repos || [];
      els.infraMeta.textContent = `${repos.length} repo(s)`;
      if (repos.length === 0) {
        els.infraContent.innerHTML = '<div class="muted">No connected git repositories yet.</div>';
        return;
      }
      els.infraContent.innerHTML = `
        <div class="table-wrap">
          <table>
            <thead><tr><th>Repo</th><th>Project</th><th>Branch</th><th>Last Commit</th><th>Local Path</th></tr></thead>
            <tbody>
              ${repos
                .map(
                  (g) => `
                  <tr>
                    <td class="mono"><strong>${esc(g.name)}</strong></td>
                    <td class="mono">${esc(g.channel)}</td>
                    <td class="mono">${esc(g.last_branch || g.default_branch || '—')}</td>
                    <td class="mono small">${esc(g.last_commit ? g.last_commit.slice(0, 12) : '—')}</td>
                    <td class="mono small muted">${esc(g.local_path)}</td>
                  </tr>`
                )
                .join('')}
            </tbody>
          </table>
        </div>
      `;
    } else if (state.infraTab === 'locks') {
      const locks = source.locks || [];
      els.infraMeta.textContent = `${locks.length} active lock(s)`;
      if (locks.length === 0) {
        els.infraContent.innerHTML = '<div class="muted">No active equipment or branch locks.</div>';
        return;
      }
      els.infraContent.innerHTML = `
        <div class="table-wrap">
          <table>
            <thead><tr><th>Resource</th><th>Holder</th><th>Channel</th><th>Expires</th><th>Note</th></tr></thead>
            <tbody>
              ${locks
                .map(
                  (l) => `
                  <tr>
                    <td class="mono"><strong>${esc(l.resource)}</strong></td>
                    <td>@${esc(l.owner_nick)}</td>
                    <td class="mono">${esc(l.channel)}</td>
                    <td class="small muted">${esc(fmtTime(l.expires_at))}</td>
                    <td>${esc(l.note || '—')}</td>
                  </tr>`
                )
                .join('')}
            </tbody>
          </table>
        </div>
      `;
    } else {
      const channels = source.channels || [];
      els.infraMeta.textContent = `${channels.length} channel(s)`;
      if (channels.length === 0) {
        els.infraContent.innerHTML = '<div class="muted">No channel activity recorded yet.</div>';
        return;
      }
      els.infraContent.innerHTML = `
        <div class="table-wrap">
          <table>
            <thead><tr><th>Channel</th><th>Messages</th><th>Last Activity</th>${state.snapshot ? '<th>Preview</th>' : ''}</tr></thead>
            <tbody>
              ${channels
                .map(
                  (c) => `
                  <tr>
                    <td class="mono"><strong>${esc(c.channel)}</strong></td>
                    <td>${esc(c.message_count)}</td>
                    <td class="small muted">${esc(fmtRelative(c.last_activity_at))}</td>
                    ${
                      state.snapshot
                        ? `<td><button type="button" class="link-btn preview-chan-btn" data-channel="${esc(c.channel)}">View recent</button></td>`
                        : ''
                    }
                  </tr>`
                )
                .join('')}
            </tbody>
          </table>
        </div>
        <div id="channelPreviewBox"></div>
      `;

      els.infraContent.querySelectorAll('.preview-chan-btn').forEach((btn) => {
        btn.addEventListener('click', () => loadChannelPreview(btn.getAttribute('data-channel') || ''));
      });
      if (state.selectedChannel && state.channelMessages) {
        renderChannelPreviewBox();
      }
    }
  }

  async function loadChannelPreview(channel) {
    if (!channel || !state.token) return;
    state.selectedChannel = channel;
    try {
      const res = await apiFetch(`/v1/messages?channel=${encodeURIComponent(channel)}&limit=20`, true);
      state.channelMessages = res.messages || [];
      renderChannelPreviewBox();
    } catch (err) {
      const box = document.getElementById('channelPreviewBox');
      if (box) box.innerHTML = `<div class="msg-preview small" style="color:var(--danger)">Failed to load ${esc(channel)}: ${esc(err.message)}</div>`;
    }
  }

  function renderChannelPreviewBox() {
    const box = document.getElementById('channelPreviewBox');
    if (!box) return;
    const msgs = state.channelMessages || [];
    box.innerHTML = `
      <div class="msg-preview">
        <div class="section-head" style="margin:0">
          <strong class="mono">${esc(state.selectedChannel)} — Recent Messages</strong>
          <button type="button" class="btn-ghost small" id="closeChanPreview">Close</button>
        </div>
        ${
          msgs.length === 0
            ? '<div class="muted small">No messages in this channel.</div>'
            : msgs
                .slice(-20)
                .map(
                  (m) => `
                  <div class="small">
                    <span class="muted mono">[#${esc(m.seq)} ${esc(fmtRelative(m.created_at))}]</span>
                    <strong>@${esc(m.sender_nick || m.sender_id)}</strong>: ${esc(m.body)}
                  </div>`
                )
                .join('')
        }
      </div>
    `;
    const closeBtn = document.getElementById('closeChanPreview');
    if (closeBtn) {
      closeBtn.addEventListener('click', () => {
        state.selectedChannel = '';
        state.channelMessages = null;
        box.innerHTML = '';
      });
    }
  }

  async function openTaskModal(taskId) {
    els.taskModal.hidden = false;
    els.modalSubtitle.textContent = taskId;
    els.modalTitle.textContent = 'Loading task...';
    els.modalBody.innerHTML = '<div class="muted">Fetching task details...</div>';

    try {
      const data = await apiFetch(`/v1/tasks/${encodeURIComponent(taskId)}`, true);
      const t = data.task;
      const events = data.events || [];
      const q = data.question;
      const subtasks = data.subtasks || [];
      const attachments = data.attachments || [];
      const issueHref = safeHttpUrl(t.issue_url);

      els.modalSubtitle.textContent = `${t.id} · ${t.channel}${t.sprint ? ' · ' + t.sprint : ''}`;
      els.modalTitle.textContent = t.title;

      els.modalBody.innerHTML = `
        <div class="detail-grid">
          <div class="detail-item"><span>Status</span><span><span class="badge ${statusBadgeClass(t.status)}">${esc(t.status)}</span></span></div>
          <div class="detail-item"><span>Kind</span><span><span class="badge ${kindBadgeClass(t.kind)}">${esc(t.kind)}</span></span></div>
          <div class="detail-item"><span>Assignee</span><span>${t.assignee ? '@' + esc(t.assignee) : (t.claimed_by ? '@' + esc(t.claimed_by) : '—')}</span></div>
          <div class="detail-item"><span>Reviewer</span><span>${t.reviewer ? '@' + esc(t.reviewer) : '—'}</span></div>
          <div class="detail-item"><span>Repo / Branch</span><span class="mono">${esc(t.repo || '—')} / ${esc(t.branch || '—')}</span></div>
          <div class="detail-item"><span>Updated</span><span>${esc(fmtTime(t.updated_at))}</span></div>
          ${t.parent_task_id ? `<div class="detail-item"><span>Parent Task</span><span><button type="button" class="link-btn modal-task-link" data-task-id="${esc(t.parent_task_id)}">${esc(t.parent_task_id)}</button></span></div>` : ''}
          ${issueHref ? `<div class="detail-item"><span>Issue</span><span><a href="${esc(issueHref)}" target="_blank" rel="noopener noreferrer">${esc(issueHref)}</a></span></div>` : ''}
        </div>

        ${
          q
            ? `<div>
                <h3>Pending Question (${esc(q.status)})</h3>
                <pre class="box" style="margin-top:6px;border-color:var(--danger)">Asked by @${esc(q.asked_by)} (${esc(fmtTime(q.created_at))}):\n${esc(q.question)}${q.escalation_note ? '\n\nEscalation: ' + esc(q.escalation_note) : ''}</pre>
              </div>`
            : ''
        }

        ${
          t.body
            ? `<div>
                <h3>Description</h3>
                <pre class="box" style="margin-top:6px">${esc(t.body)}</pre>
              </div>`
            : ''
        }

        ${
          t.review_round
            ? `<div>
                <h3>Peer Review Round (${esc(t.review_round.revision)})</h3>
                <div class="timeline" style="margin-top:6px">
                  <div class="timeline-item">
                    <div class="small muted">Group: <code>${esc(t.review_round.group)}</code> · Commit: <code>${esc(t.review_round.commit)}</code> · Quorum: ${esc(t.review_round.quorum)}</div>
                    <div class="small">${esc(t.review_round.evidence)}</div>
                    ${(t.review_round.votes || []).map((v) => `<div class="small">@${esc(v.reviewer)}: <strong>${esc(v.decision)}</strong> — ${esc(v.note)}</div>`).join('')}
                  </div>
                </div>
              </div>`
            : ''
        }

        ${
          subtasks.length > 0
            ? `<div>
                <h3>Subtasks (${subtasks.length})</h3>
                <div class="timeline" style="margin-top:6px">
                  ${subtasks
                    .map(
                      (st) => `
                      <div class="timeline-item">
                        <div style="display:flex;justify-content:space-between;gap:8px">
                          <button type="button" class="link-btn modal-task-link" data-task-id="${esc(st.id)}">${esc(st.title)}</button>
                          <span class="badge ${statusBadgeClass(st.status)}">${esc(st.status)}</span>
                        </div>
                        <div class="small muted mono">${esc(st.id)}${st.assignee ? ' · @' + esc(st.assignee) : ''}</div>
                      </div>`
                    )
                    .join('')}
                </div>
              </div>`
            : ''
        }

        ${
          attachments.length > 0
            ? `<div>
                <h3>Attachments (${attachments.length})</h3>
                <div class="timeline" style="margin-top:6px">
                  ${attachments
                    .map(
                      (att) => `
                      <div class="timeline-item small">
                        <div><strong>${esc(att.filename)}</strong> (${esc(att.kind)}, ${esc(att.size_bytes)} B) by @${esc(att.uploaded_by)}</div>
                        ${att.note ? `<div class="muted">${esc(att.note)}</div>` : ''}
                      </div>`
                    )
                    .join('')}
                </div>
              </div>`
            : ''
        }

        <div>
          <h3>History (${events.length})</h3>
          <div class="timeline" style="margin-top:6px">
            ${
              events.length === 0
                ? '<div class="muted small">No task events yet.</div>'
                : events
                    .map(
                      (ev) => `
                      <div class="timeline-item">
                        <div class="small">
                          <strong>${esc(ev.action)}</strong>
                          ${ev.from_status || ev.to_status ? `<code>${esc(ev.from_status || '?')} &rarr; ${esc(ev.to_status || '?')}</code>` : ''}
                          by <strong>@${esc(ev.actor_id)}</strong>
                          <span class="muted">&middot; ${esc(fmtTime(ev.created_at))}</span>
                        </div>
                        ${ev.note ? `<pre class="box">${esc(ev.note)}</pre>` : ''}
                      </div>`
                    )
                    .join('')
            }
          </div>
        </div>
      `;

      els.modalBody.querySelectorAll('.modal-task-link').forEach((btn) => {
        btn.addEventListener('click', () => {
          const nextId = btn.getAttribute('data-task-id');
          if (nextId) openTaskModal(nextId);
        });
      });
    } catch (err) {
      els.modalTitle.textContent = 'Error loading task';
      els.modalBody.innerHTML = `<div class="small" style="color:var(--danger)">${esc(err.message)}</div>`;
    }
  }

  function closeTaskModal() {
    els.taskModal.hidden = true;
  }

  function renderAll() {
    syncFilterOptions();
    renderKpis();
    renderSprints();
    renderTasks();
    renderAgents();
    renderInfra();
  }

  function resetAutoRefresh() {
    if (state.timerId) {
      clearInterval(state.timerId);
      state.timerId = null;
    }
    if (state.refreshMs > 0) {
      state.timerId = setInterval(() => {
        if (!document.hidden && !state.authFailed) {
          loadData();
        }
      }, state.refreshMs);
    }
  }

  function syncAuthControls() {
    els.tokenInput.value = state.token;
    els.disconnectBtn.hidden = !state.token;
  }

  function bindEvents() {
    els.toggleTokenBtn.addEventListener('click', () => {
      const showing = els.tokenInput.type === 'text';
      els.tokenInput.type = showing ? 'password' : 'text';
      els.toggleTokenBtn.textContent = showing ? 'Show' : 'Hide';
    });

    const connectToken = () => {
      const tok = els.tokenInput.value.trim();
      state.token = tok;
      state.authFailed = false;
      if (tok) {
        localStorage.setItem(TOKEN_STORAGE_KEY, tok);
      } else {
        localStorage.removeItem(TOKEN_STORAGE_KEY);
      }
      syncAuthControls();
      loadData();
    };

    els.connectBtn.addEventListener('click', connectToken);
    els.tokenInput.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') connectToken();
    });

    els.disconnectBtn.addEventListener('click', () => {
      state.token = '';
      state.authFailed = false;
      state.selectedChannel = '';
      state.channelMessages = null;
      localStorage.removeItem(TOKEN_STORAGE_KEY);
      syncAuthControls();
      loadData();
    });

    els.projectFilter.addEventListener('change', () => {
      state.project = els.projectFilter.value;
      state.sprint = '';
      loadData();
    });

    els.sprintFilter.addEventListener('change', () => {
      state.sprint = els.sprintFilter.value;
      loadData();
    });

    els.searchInput.addEventListener('input', () => {
      state.search = els.searchInput.value;
      renderTasks();
    });

    els.includeTerminal.addEventListener('change', () => {
      state.includeTerminal = els.includeTerminal.checked;
      loadData();
    });

    const setViewMode = (mode) => {
      state.viewMode = mode;
      localStorage.setItem(VIEW_STORAGE_KEY, mode);
      els.viewBoardBtn.classList.toggle('active', mode === 'board');
      els.viewTableBtn.classList.toggle('active', mode === 'table');
      renderTasks();
    };
    els.viewBoardBtn.addEventListener('click', () => setViewMode('board'));
    els.viewTableBtn.addEventListener('click', () => setViewMode('table'));
    els.viewBoardBtn.classList.toggle('active', state.viewMode === 'board');
    els.viewTableBtn.classList.toggle('active', state.viewMode === 'table');

    els.refreshInterval.addEventListener('change', () => {
      state.refreshMs = Number(els.refreshInterval.value) || 0;
      resetAutoRefresh();
    });

    els.refreshBtn.addEventListener('click', loadData);

    document.querySelectorAll('#infraSection [data-tab]').forEach((btn) => {
      btn.addEventListener('click', () => {
        state.infraTab = btn.getAttribute('data-tab') || 'repos';
        renderInfra();
      });
    });

    els.closeModalBtn.addEventListener('click', closeTaskModal);
    els.taskModal.addEventListener('click', (e) => {
      if (e.target === els.taskModal) closeTaskModal();
    });
    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape' && !els.taskModal.hidden) closeTaskModal();
    });
  }

  syncAuthControls();
  bindEvents();
  resetAutoRefresh();
  loadData();
})();
