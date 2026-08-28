'use strict';

const tauri = window.__TAURI__;
if (!tauri?.core?.invoke || !tauri?.event?.listen) {
  document.body.textContent = 'MCP 活动窗口需要由 PXLogic Bridge 启动';
} else {
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;
  const elements = {
    handle: document.querySelector('#drag-handle'),
    hide: document.querySelector('#hide-button'),
    refresh: document.querySelector('#refresh-button'),
    dot: document.querySelector('#proxy-dot'),
    summary: document.querySelector('#proxy-summary'),
    endpoint: document.querySelector('#proxy-endpoint'),
    upstream: document.querySelector('#upstream-status'),
    upstreamHelp: document.querySelector('#upstream-help'),
    fallback: document.querySelector('#fallback-warning'),
    registration: document.querySelector('#registration-value'),
    copy: document.querySelector('#copy-button'),
    autoShow: document.querySelector('#auto-show'),
    toolCount: document.querySelector('#tool-count'),
    toolList: document.querySelector('#tool-list'),
    activityCount: document.querySelector('#activity-count'),
    activityList: document.querySelector('#activity-list'),
    approvalSection: document.querySelector('#approval-section'),
    approvalCount: document.querySelector('#approval-count'),
    approvalList: document.querySelector('#approval-list'),
  };
  const activities = new Map();
  const approvals = new Map();
  let tools = [];

  function renderStatus(status) {
    const port = status?.listenPort;
    const endpoint = port ? `http://127.0.0.1:${port}/` : '等待绑定';
    elements.endpoint.textContent = endpoint;
    elements.registration.textContent = endpoint;
    elements.autoShow.checked = Boolean(status?.autoShow);
    elements.upstream.textContent = status?.upstreamReachable ? '已连接' : '未连接';
    elements.upstream.className = status?.upstreamReachable ? 'up' : 'down';
    elements.upstreamHelp.hidden = Boolean(status?.upstreamReachable);
    elements.dot.className = `state-dot ${status?.upstreamReachable ? 'ready' : 'warning'}`;
    elements.summary.textContent = port
      ? `127.0.0.1:${port} → Logic 2 :${status.upstreamPort}`
      : '代理尚未绑定';
    elements.fallback.hidden = !status?.fellBack;
    elements.fallback.textContent = status?.fellBack
      ? `首选端口 ${status.requestedListenPort} 已被占用，当前临时使用 ${port}。MCP 客户端必须改用上面的实际 URL。`
      : '';
  }

  function renderTools(nextTools) {
    tools = Array.isArray(nextTools) ? nextTools : [];
    elements.toolCount.textContent = tools.length ? `${tools.length} 项` : '尚未读取';
    if (!tools.length) {
      const empty = document.createElement('p');
      empty.className = 'empty';
      empty.textContent = 'agent 请求 tools/list 后显示真实工具清单。';
      elements.toolList.replaceChildren(empty);
      return;
    }
    elements.toolList.replaceChildren(...tools.map(tool => {
      const chip = document.createElement('span');
      chip.className = 'tool-chip';
      chip.textContent = tool.name;
      chip.title = tool.description || tool.name;
      return chip;
    }));
  }

  function activityName(activity) {
    if (activity.tool) return activity.tool;
    if (activity.method === 'response') return `响应 ${JSON.stringify(activity.requestId)}`;
    return activity.method || 'MCP 消息';
  }

  function renderActivities() {
    const ordered = [...activities.values()].sort((a, b) => b.sequence - a.sequence);
    elements.activityCount.textContent = `${ordered.length} 条`;
    if (!ordered.length) {
      const empty = document.createElement('p');
      empty.className = 'empty';
      empty.textContent = '尚无 MCP 活动。';
      elements.activityList.replaceChildren(empty);
      return;
    }
    elements.activityList.replaceChildren(...ordered.map(activity => {
      const row = document.createElement('article');
      row.className = 'activity';
      const top = document.createElement('div');
      top.className = 'activity-top';
      const state = document.createElement('span');
      state.className = `activity-state ${activity.state || ''}`;
      const name = document.createElement('span');
      name.className = 'activity-name';
      name.textContent = activityName(activity);
      const time = document.createElement('time');
      time.className = 'activity-time';
      time.textContent = new Date(activity.observedAtMs || 0).toLocaleTimeString([], {
        hour: '2-digit', minute: '2-digit', second: '2-digit',
      });
      top.append(state, name, time);
      const meta = document.createElement('p');
      meta.className = 'activity-meta';
      const requestId = activity.requestId == null ? '通知' : `id ${JSON.stringify(activity.requestId)}`;
      meta.textContent = `${activity.state || 'observed'} · ${requestId}${activity.sessionId ? ` · ${activity.sessionId}` : ''}`;
      row.append(top, meta);
      const detailValue = activity.response ?? activity.arguments ?? activity.params;
      if (detailValue != null) {
        const detail = document.createElement('details');
        const summary = document.createElement('summary');
        summary.textContent = activity.response ? '查看响应' : '查看参数';
        const pre = document.createElement('pre');
        pre.textContent = JSON.stringify(detailValue, null, 2);
        detail.append(summary, pre);
        row.append(detail);
      }
      return row;
    }));
  }

  function upsertActivity(activity) {
    if (!activity || !Number.isFinite(Number(activity.sequence))) return;
    activities.set(Number(activity.sequence), activity);
    while (activities.size > 200) activities.delete(Math.min(...activities.keys()));
    renderActivities();
  }

  async function resolveApproval(approval, allow, remember, card) {
    for (const button of card.querySelectorAll('button')) button.disabled = true;
    try {
      await invoke('mcp_approval_resolve', {
        approvalId: approval.approvalId,
        allow,
        remember: allow && remember,
      });
    } catch (error) {
      const message = card.querySelector('.approval-error');
      message.textContent = String(error);
      for (const button of card.querySelectorAll('button')) button.disabled = false;
    }
  }

  function renderApprovals() {
    const pending = [...approvals.values()].sort((a, b) => a.approvalId - b.approvalId);
    elements.approvalSection.hidden = pending.length === 0;
    elements.approvalCount.textContent = `${pending.length} 项`;
    elements.approvalList.replaceChildren(...pending.map(approval => {
      const card = document.createElement('article');
      card.className = 'approval-card';
      card.dataset.approvalId = String(approval.approvalId);
      const heading = document.createElement('div');
      heading.className = 'approval-heading';
      const name = document.createElement('strong');
      name.textContent = approval.tool;
      const countdown = document.createElement('span');
      countdown.className = 'approval-countdown';
      const seconds = Math.max(0, Math.ceil((Number(approval.expiresAtMs) - Date.now()) / 1000));
      countdown.textContent = `${seconds} 秒后拒绝`;
      heading.append(name, countdown);
      const reason = document.createElement('p');
      reason.className = 'approval-reason';
      reason.textContent = approval.reason;
      const args = document.createElement('pre');
      args.textContent = JSON.stringify(approval.arguments ?? {}, null, 2);
      const rememberLabel = document.createElement('label');
      rememberLabel.className = 'approval-remember';
      const remember = document.createElement('input');
      remember.type = 'checkbox';
      remember.disabled = !approval.sessionId;
      const rememberText = document.createElement('span');
      rememberText.textContent = approval.sessionId
        ? '本次 MCP 会话内该工具免问'
        : '尚无会话 ID，不能记住';
      rememberLabel.append(remember, rememberText);
      const actions = document.createElement('div');
      actions.className = 'approval-actions';
      const deny = document.createElement('button');
      deny.type = 'button';
      deny.className = 'deny-button';
      deny.textContent = '拒绝';
      const allow = document.createElement('button');
      allow.type = 'button';
      allow.className = 'allow-button';
      allow.textContent = '允许';
      deny.addEventListener('click', () => void resolveApproval(approval, false, false, card));
      allow.addEventListener('click', () => void resolveApproval(approval, true, remember.checked, card));
      actions.append(deny, allow);
      const error = document.createElement('p');
      error.className = 'approval-error';
      card.append(heading, reason, args, rememberLabel, actions, error);
      return card;
    }));
  }

  async function refresh() {
    const [status, snapshot] = await Promise.all([
      invoke('mcp_status'),
      invoke('mcp_activity_snapshot'),
    ]);
    renderStatus(status);
    activities.clear();
    for (const activity of snapshot?.activities || []) activities.set(Number(activity.sequence), activity);
    renderActivities();
    renderTools(snapshot?.tools);
    approvals.clear();
    for (const approval of snapshot?.approvals || []) approvals.set(Number(approval.approvalId), approval);
    renderApprovals();
  }

  async function copyEndpoint() {
    const value = elements.registration.textContent;
    if (!value || value === '等待绑定') return;
    try {
      await navigator.clipboard.writeText(value);
      elements.copy.textContent = '已复制';
    } catch (_) {
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(elements.registration);
      selection.removeAllRanges();
      selection.addRange(range);
      elements.copy.textContent = '已选中';
    }
    setTimeout(() => { elements.copy.textContent = '复制 URL'; }, 1200);
  }

  const DRAG_THRESHOLD = 6;
  let dragOrigin = null;
  let moving = false;
  let movePending = false;
  function requestMove() {
    if (movePending) return;
    movePending = true;
    void invoke('mcp_window_move').catch(() => {}).finally(() => { movePending = false; });
  }
  function endMove() {
    dragOrigin = null;
    if (!moving) return;
    moving = false;
    void invoke('mcp_window_end_move').catch(() => {});
  }
  elements.handle.addEventListener('mousedown', event => {
    if (event.button !== 0 || event.target.closest('button')) return;
    dragOrigin = { x: event.screenX, y: event.screenY };
  });
  window.addEventListener('mousemove', event => {
    if (moving) {
      if (event.buttons & 1) requestMove(); else endMove();
      return;
    }
    if (!dragOrigin || !(event.buttons & 1)) return;
    if (Math.abs(event.screenX - dragOrigin.x) < DRAG_THRESHOLD &&
        Math.abs(event.screenY - dragOrigin.y) < DRAG_THRESHOLD) return;
    dragOrigin = null;
    moving = true;
    void invoke('mcp_window_begin_move').then(requestMove).catch(() => { moving = false; });
  });
  window.addEventListener('mouseup', endMove);
  window.addEventListener('blur', endMove);

  elements.hide.addEventListener('click', () => void invoke('mcp_window_hide'));
  elements.refresh.addEventListener('click', () => void refresh().catch(() => {}));
  elements.copy.addEventListener('click', () => void copyEndpoint());
  elements.autoShow.addEventListener('change', () => {
    void invoke('mcp_set_auto_show', { enabled: elements.autoShow.checked }).catch(() => {
      elements.autoShow.checked = !elements.autoShow.checked;
    });
  });
  void listen('mcp-status', event => renderStatus(event.payload));
  void listen('mcp-activity', event => upsertActivity(event.payload));
  void listen('mcp-tools', event => renderTools(event.payload));
  void listen('mcp-approval', event => {
    const approval = event.payload;
    approvals.set(Number(approval.approvalId), approval);
    renderApprovals();
  });
  void listen('mcp-approval-resolved', event => {
    approvals.delete(Number(event.payload?.approvalId));
    renderApprovals();
  });
  setInterval(() => {
    for (const card of elements.approvalList.querySelectorAll('.approval-card')) {
      const approval = approvals.get(Number(card.dataset.approvalId));
      const countdown = card.querySelector('.approval-countdown');
      if (!approval || !countdown) continue;
      const seconds = Math.max(0, Math.ceil((Number(approval.expiresAtMs) - Date.now()) / 1000));
      countdown.textContent = `${seconds} 秒后拒绝`;
    }
  }, 1000);
  void refresh().catch(error => { elements.summary.textContent = String(error); });
}
