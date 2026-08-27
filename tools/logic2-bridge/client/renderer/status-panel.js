'use strict';

const tauri = window.__TAURI__;
if (!tauri?.core?.invoke || !tauri?.event?.listen) {
  document.body.textContent = '状态面板需要由 PXLogic Bridge 启动';
} else {
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;
  const elements = {
    hide: document.querySelector('#hide-button'),
    collapse: document.querySelector('#collapse-button'),
    header: document.querySelector('.panel-header'),
    chip: document.querySelector('#panel-chip'),
    chipDot: document.querySelector('#chip-dot'),
    chipLabel: document.querySelector('#chip-label'),
    intro: document.querySelector('#panel-intro'),
    introDismiss: document.querySelector('#panel-intro-dismiss'),
    introDisable: document.querySelector('#panel-intro-disable'),
    main: document.querySelector('#main-button'),
    stop: document.querySelector('#stop-button'),
    dot: document.querySelector('#state-dot'),
    state: document.querySelector('#state-label'),
    detail: document.querySelector('#state-detail'),
    channelGrid: document.querySelector('#channel-grid'),
    logicRate: document.querySelector('#logic-rate'),
    effectiveRate: document.querySelector('#effective-rate'),
    mode: document.querySelector('#stream-mode'),
    threshold: document.querySelector('#threshold'),
    usb: document.querySelector('#usb-speed'),
    quality: document.querySelector('#quality-label'),
    fill: document.querySelector('#quality-fill'),
    qualityDetail: document.querySelector('#quality-detail'),
    device: document.querySelector('#device-label'),
    serial: document.querySelector('#device-serial'),
    endpoint: document.querySelector('#endpoint'),
    body: document.querySelector('main'),
    footer: document.querySelector('footer'),
  };
  let hardware = null;
  let telemetry = null;
  let bridgeState = null;
  let collapsed = false;

  const formatRate = rate => {
    if (!Number.isFinite(Number(rate))) return '--';
    const value = Number(rate);
    if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(3))} MHz`;
    if (value >= 1_000) return `${Number((value / 1_000).toFixed(3))} kHz`;
    return `${value} Hz`;
  };
  const formatBytes = bytes => {
    if (!Number.isFinite(Number(bytes))) return '--';
    const value = Number(bytes);
    if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(1)} MiB`;
    if (value >= 1024) return `${(value / 1024).toFixed(1)} KiB`;
    return `${value} B`;
  };
  const usbLabel = speed => ({
    high: 'USB 2.0 High-Speed', super: 'USB 3.x SuperSpeed',
    full: 'USB 1.x Full-Speed', low: 'USB 1.x Low-Speed',
  }[String(speed || '').toLowerCase()] || speed || '--');

  function selectedDevice() {
    return hardware?.devices?.find(device => device.id === hardware.selectedDeviceId) || hardware?.devices?.[0];
  }

  // Ported from Logic 2.4.43 app/services/themes/builtin.ts, the same eight colours
  // Logic 2 paints its own channels with, so a channel is the same colour here as it
  // is in the waveform the user is looking at. The palette repeats every eight.
  const CHANNEL_COLORS = [
    '#d4d4d4', '#C79579', '#FF6D7F', '#FFB45B',
    '#e8d836', '#58c667', '#53A9FD', '#AF92FB',
  ];
  // Logic Pro 16 is what the Bridge presents, so sixteen digital channels is the set
  // Logic 2 can offer. Kept as a floor rather than a fixed size: should a capture
  // ever report a higher index, the grid grows to a whole number of rows instead of
  // quietly omitting an enabled channel.
  const CHANNEL_GRID_COLUMNS = 8;
  const CHANNEL_GRID_MINIMUM = 16;

  function channelGridSize(enabled) {
    const highest = enabled.reduce((top, channel) => Math.max(top, Number(channel) + 1), 0);
    const needed = Math.max(CHANNEL_GRID_MINIMUM, highest);
    return Math.ceil(needed / CHANNEL_GRID_COLUMNS) * CHANNEL_GRID_COLUMNS;
  }

  // Rebuilt only when the channel count changes; the cells themselves are cheap to
  // re-mark and rebuilding on every telemetry tick would discard nothing useful.
  function renderChannels(enabled) {
    const size = channelGridSize(enabled);
    if (elements.channelGrid.childElementCount !== size) {
      elements.channelGrid.replaceChildren(...Array.from({ length: size }, (_, index) => {
        const cell = document.createElement('span');
        cell.className = 'channel-cell';
        cell.style.setProperty('--channel-color', CHANNEL_COLORS[index % CHANNEL_COLORS.length]);
        cell.textContent = String(index);
        return cell;
      }));
    }
    const on = new Set(enabled.map(Number));
    for (const [index, cell] of [...elements.channelGrid.children].entries()) {
      cell.classList.toggle('on', on.has(index));
    }
    // The grid is a picture, so the reading of it has to be spelled out for anything
    // that cannot see colour or position.
    elements.channelGrid.setAttribute(
      'aria-label',
      on.size ? `已启用 ${[...on].sort((a, b) => a - b).map(channel => `D${channel}`).join('、')}` : '未启用任何通道',
    );
  }

  // The state label already reads 已连接 or 未连接, so this line only carries what
  // the label cannot: an error code from the Bridge or a failed command. Calling
  // it with nothing hides it rather than filling the row with a restatement.
  function showProblem(text) {
    const message = text ? String(text) : '';
    elements.detail.textContent = message;
    // Two lines are shown; hovering reveals the rest.
    elements.detail.title = message;
    elements.detail.hidden = !message;
  }

  function renderState(state) {
    bridgeState = state;
    const phase = state?.phase || 'stopped';
    elements.dot.className = `state-dot ${phase}`;
    elements.state.textContent = state?.message || '待机';
    showProblem(state?.errorCode);
    // Nothing to stop unless a session is up, and a stale armed button must not
    // survive the session it was aimed at.
    const live = phase !== 'stopped' && phase !== 'error';
    elements.stop.hidden = !live;
    if (!live) disarmStop();
    elements.endpoint.textContent = state?.actualPort ? `WS ${state.actualPort}` : 'WebSocket --';
    elements.chipDot.className = `state-dot ${phase}`;
    renderChipLabel();
  }

  // The chip has room for one string. While capturing, the injected total is the
  // only number that shows data is still flowing; otherwise the phase message is
  // what the user needs. The dot carries the phase colour in both cases.
  function renderChipLabel() {
    const injected = Number(telemetry?.injectedBytes || 0);
    elements.chipLabel.textContent = injected > 0
      ? `已注入 ${formatBytes(injected)}`
      : bridgeState?.message || '待机';
    elements.chip.title = `${bridgeState?.message || '待机'}｜${usbLabel(
      telemetry?.pxlogicUsbSpeed || selectedDevice()?.usbSpeed,
    )}｜点击展开状态面板`;
  }

  function applyCollapsed(next) {
    collapsed = Boolean(next);
    document.body.classList.toggle('collapsed', collapsed);
    elements.collapse.setAttribute('aria-expanded', String(!collapsed));
    renderChipLabel();
    // Expanding reveals sections that were display:none and therefore unmeasurable.
    if (!collapsed) fitToContent();
  }

  // Resting on the bottom edge puts the header there too, so the drag handle and
  // the collapse control stay on the edge the panel is docked against. The Bridge
  // decides this: it owns the work-area geometry and the snap tolerance.
  function applyDock(bottom) {
    document.body.classList.toggle('dock-bottom', Boolean(bottom));
  }

  // The window is sized to the readout rather than scrolled. Only the renderer can
  // measure it: the height depends on how the text wrapped, and the first-run card
  // and an error line each add a chunk. The three sections are measured directly
  // instead of the document, whose height is capped by the window being corrected.
  let fitPending = 0;
  function fitToContent() {
    if (collapsed) return;
    cancelAnimationFrame(fitPending);
    fitPending = requestAnimationFrame(() => {
      const height = [elements.header, elements.body, elements.footer].reduce(
        (total, node) => total + (node ? node.getBoundingClientRect().height : 0),
        0,
      );
      if (height > 0) void invoke('status_panel_fit_height', { height: Math.ceil(height) }).catch(() => {});
    });
  }

  async function setCollapsed(next) {
    applyCollapsed(next);
    try {
      await invoke('status_panel_set_collapsed', { collapsed });
    } catch (error) {
      showProblem(error?.message || error);
    }
  }

  // With no native titlebar the panel has to move itself. A press turns into a
  // drag only after the pointer travels with the button still held, so the chip
  // stays clickable and the header still receives its double-click. Without the
  // `buttons` check a stray pointer move — the system delivers them around window
  // activation — would be mistaken for a drag.
  const DRAG_THRESHOLD = 6;

  function bindDragHandle(handle, onClick) {
    let origin = null;
    let dragged = false;
    let moving = false;
    // One move request in flight at a time. The Bridge reads the live cursor on
    // every step rather than trusting coordinates from here, so skipping a move
    // costs nothing while queueing them would only add latency to the last one.
    let pending = false;

    function requestMove() {
      if (pending) return;
      pending = true;
      void invoke('status_panel_move')
        .catch(() => {})
        .finally(() => {
          pending = false;
        });
    }

    function endMove() {
      origin = null;
      if (!moving) return;
      moving = false;
      void invoke('status_panel_end_move').catch(() => {});
    }

    handle.addEventListener('mousedown', event => {
      if (event.button !== 0) return;
      // The chip is itself a button, so only nested controls block a drag.
      const control = event.target.closest('button');
      if (control && control !== handle) return;
      origin = { x: event.screenX, y: event.screenY };
      dragged = false;
    });
    window.addEventListener('mousemove', event => {
      if (moving) {
        // Cocoa keeps delivering moves to the view that received the press even
        // once the pointer leaves the window, but a release out there does not
        // always come back as `mouseup`; a move without the button held is the
        // one end-of-drag signal that always arrives.
        if (event.buttons & 1) requestMove();
        else endMove();
        return;
      }
      if (!origin) return;
      if (!(event.buttons & 1)) return;
      if (Math.abs(event.screenX - origin.x) < DRAG_THRESHOLD &&
          Math.abs(event.screenY - origin.y) < DRAG_THRESHOLD) return;
      origin = null;
      dragged = true;
      moving = true;
      // The Bridge moves the window itself instead of handing the gesture to
      // macOS, which would arm the system's edge tiling and offer to zoom a
      // 340 px monitor the moment it touches a screen edge.
      void invoke('status_panel_begin_move').then(requestMove).catch(() => {
        moving = false;
      });
    });
    window.addEventListener('mouseup', endMove);
    // A drag interrupted by losing the window has to release its anchor too.
    window.addEventListener('blur', endMove);
    if (!onClick) return;
    handle.addEventListener('click', () => {
      if (dragged) {
        dragged = false;
        return;
      }
      onClick();
    });
  }

  // The panel can appear on its own once the Bridge goes live, so the first
  // reveal explains what it is and how to get rid of it.
  async function dismissIntro(disableAutoShow) {
    elements.intro.hidden = true;
    try {
      if (disableAutoShow) await invoke('status_panel_set_auto_show', { enabled: false });
      await invoke('status_panel_intro_acknowledge');
    } catch (error) {
      showProblem(error?.message || error);
    }
  }

  // The comparator threshold is the one setting the panel can change. It used to be
  // fixed at launch, so a wrong guess could only be corrected by closing Logic 2 and
  // losing the capture in it. `appliedThreshold` is what the Bridge is actually using,
  // kept so a rejected edit can be put back rather than left claiming something else.
  let appliedThreshold = null;

  function renderThreshold(volts) {
    appliedThreshold = Number.isFinite(Number(volts)) ? Number(volts) : null;
    // Never overwrite a value the user is in the middle of typing.
    if (document.activeElement === elements.threshold) return;
    elements.threshold.value = appliedThreshold === null ? '' : appliedThreshold.toFixed(3);
  }

  function setThresholdEditable(capturing) {
    elements.threshold.disabled = capturing;
    elements.threshold.title = capturing
      ? '采集进行中无法修改，请先在 Logic 2 里停止采集'
      : '修改后在 Logic 2 下一次采集时生效';
  }

  async function applyThreshold() {
    const volts = Number(elements.threshold.value);
    if (!Number.isFinite(volts)) {
      renderThreshold(appliedThreshold);
      return;
    }
    try {
      const accepted = await invoke('status_panel_set_threshold', { volts });
      appliedThreshold = Number(accepted);
      elements.threshold.value = appliedThreshold.toFixed(3);
      showProblem('');
    } catch (error) {
      // Put back what is actually in force, so the field never claims a threshold the
      // hardware is not using.
      renderThreshold(appliedThreshold);
      showProblem(error?.message || error);
    }
  }

  // Stopping the Bridge closes Logic 2, which takes any unsaved capture with it. The
  // panel floats over Logic 2 and is easy to catch by accident, so the button arms
  // first and becomes its own confirmation. It disarms on a timer so a click made and
  // thought better of does not stay loaded indefinitely.
  const STOP_ARMED_MS = 4000;
  let stopArmedTimer = 0;

  function disarmStop() {
    clearTimeout(stopArmedTimer);
    stopArmedTimer = 0;
    elements.stop.classList.remove('armed');
    elements.stop.textContent = '停止 Bridge';
    elements.stop.title = '关闭 Logic 2 并结束这次 Bridge 会话';
  }

  function armStop() {
    elements.stop.classList.add('armed');
    elements.stop.textContent = '确认停止';
    elements.stop.title = '再点一次会关闭 Logic 2，未保存的采集数据将会丢失';
    clearTimeout(stopArmedTimer);
    stopArmedTimer = setTimeout(disarmStop, STOP_ARMED_MS);
  }

  async function stopBridge() {
    if (!elements.stop.classList.contains('armed')) {
      armStop();
      return;
    }
    disarmStop();
    try {
      await invoke('bridge_stop');
    } catch (error) {
      showProblem(error?.message || error);
    }
  }

  // Logic 2 lowers its own sample rate when the enabled channels make the requested one
  // impossible, and it does not tell the GraphServer when it does: the rate it last sent
  // can sit at 500 MHz while its own UI already reads 250 MHz. The Bridge derives the
  // same clamp from the channel count and the mode table, so the derived value is what
  // is shown -- otherwise this row contradicts the window it is sitting on top of.
  //
  // The request is not thrown away, it moves to the tooltip: knowing the rate was
  // reduced, and from what, is the difference between a deliberate 250 MHz and a 500 MHz
  // that quietly did not happen.
  function renderRates(values) {
    const requested = Number(values.logicSampleRateHz ?? values.sampleRateHz);
    const effective = Number(values.pxlogicEffectiveSampleRateHz ?? values.sampleRateHz);
    const inForce = Number.isFinite(effective) ? effective : requested;
    elements.logicRate.textContent = formatRate(inForce);
    const reduced = Number.isFinite(requested) && Number.isFinite(effective) && effective < requested;
    elements.logicRate.title = reduced
      ? `Logic 2 请求 ${formatRate(requested)}，启用通道数超出该速率的上限，已降为 ${formatRate(effective)}`
      : '';
    elements.logicRate.classList.toggle('reduced', reduced);
    elements.effectiveRate.textContent = formatRate(effective);
  }

  function renderTelemetry(value) {
    telemetry = value || {};
    renderChannels(Array.isArray(telemetry.enabledChannels) ? telemetry.enabledChannels : []);
    renderRates(telemetry);
    elements.mode.textContent = telemetry.pxlogicMode || '--';
    if (Number.isFinite(Number(telemetry.thresholdVolts))) renderThreshold(telemetry.thresholdVolts);
    setThresholdEditable(['starting', 'streaming'].includes(String(telemetry.status)));
    elements.usb.textContent = usbLabel(telemetry.pxlogicUsbSpeed || selectedDevice()?.usbSpeed);
    const device = selectedDevice();
    // With no device the identity line has nothing to say, so it says that
    // instead of showing a bare placeholder next to the product name.
    if (device) {
      elements.device.textContent = device.profileModel || device.product || device.label || 'PXLogic';
      elements.serial.textContent = device.serialNumber ? `序列号 ${device.serialNumber}` : '';
      elements.serial.hidden = !device.serialNumber;
      // Both strings outgrow a 340 px panel, so hovering has to reveal them.
      elements.serial.title = device.serialNumber || '';
    } else {
      elements.device.textContent = '未检测到 PXLogic';
      elements.serial.textContent = '';
      elements.serial.hidden = true;
      elements.serial.title = '';
    }
    elements.device.title = elements.device.textContent;

    const underflows = Number(telemetry.underflows || 0);
    const dropped = Number(telemetry.droppedBytes || 0);
    const injected = Number(telemetry.injectedBytes || 0);
    const quality = telemetry.status === 'error' || dropped > 0 ? 'error' : underflows > 0 ? 'warn' : 'ok';
    elements.quality.className = quality;
    elements.quality.textContent = quality === 'error' ? '异常' : quality === 'warn' ? '需关注' : injected > 0 ? '正常' : '待统计';
    elements.fill.className = quality;
    elements.qualityDetail.textContent = injected > 0
      ? `已注入 ${formatBytes(injected)} · 下溢 ${underflows} 次 · 丢弃 ${formatBytes(dropped)}`
      : '开始采集后显示注入、下溢和丢弃统计';
    renderChipLabel();
  }

  elements.hide.addEventListener('click', () => invoke('status_panel_hide'));
  elements.main.addEventListener('click', () => invoke('main_window_show'));
  elements.stop.addEventListener('click', () => void stopBridge());
  // Establishes the resting label and its explanation before any state arrives.
  disarmStop();
  elements.collapse.addEventListener('click', () => setCollapsed(true));
  // `change` rather than `input`: it fires on blur and Enter, so a half-typed number
  // is never sent to the hardware.
  elements.threshold.addEventListener('change', () => void applyThreshold());
  elements.introDismiss.addEventListener('click', () => dismissIntro(false));
  elements.introDisable.addEventListener('click', () => dismissIntro(true));
  // Double-clicking the title area is the habit users bring from every other
  // collapsible utility panel.
  elements.header.addEventListener('dblclick', event => {
    if (event.target.closest('button')) return;
    setCollapsed(true);
  });
  bindDragHandle(elements.chip, () => void setCollapsed(false));
  bindDragHandle(elements.header, null);
  listen('bridge-state', event => renderState(event.payload));
  listen('capture-telemetry', event => renderTelemetry(event.payload));
  listen('status-panel-dock', event => applyDock(event.payload?.bottom));
  // Either window can change the threshold, and each used to read it once at load
  // and never hear about the other's change.
  listen('pxlogic-threshold', event => renderThreshold(event.payload?.volts));

  invoke('client_initial_state').then(initial => {
    hardware = initial.hardware;
    applyCollapsed(initial.settings?.statusPanel?.collapsed);
    renderThreshold(initial.settings?.pxlogicThresholdVolts);
    elements.intro.hidden = Boolean(initial.settings?.guidance?.statusPanelIntroSeen);
    renderState(initial.bridgeState);
    renderTelemetry(initial.captureTelemetry);
  }).catch(error => {
    showProblem(error?.message || error);
  });
  // The change event only fires on a change, so a panel reloaded long after its
  // last move has to ask which way round it belongs.
  invoke('status_panel_dock_edge').then(applyDock).catch(() => {});

  // Anything that changes the height goes through a layout: the error line
  // appearing, the first-run card being dismissed, a value wrapping onto a second
  // line after the user narrows the panel. Observing the sections catches all of
  // them without every render site having to remember to ask.
  if (window.ResizeObserver) {
    const observer = new ResizeObserver(() => fitToContent());
    for (const section of [elements.header, elements.body, elements.footer]) {
      if (section) observer.observe(section);
    }
  }
  fitToContent();
}
