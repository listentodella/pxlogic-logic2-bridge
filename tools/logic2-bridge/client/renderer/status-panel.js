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
    dot: document.querySelector('#state-dot'),
    state: document.querySelector('#state-label'),
    detail: document.querySelector('#state-detail'),
    channels: document.querySelector('#channels'),
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

  function renderState(state) {
    bridgeState = state;
    const phase = state?.phase || 'stopped';
    elements.dot.className = `state-dot ${phase}`;
    elements.state.textContent = state?.message || '待机';
    elements.detail.textContent = state?.errorCode || (state?.actualPort ? 'Bridge 已连接到 Logic 2' : '等待 Logic 2 启动');
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
  }

  async function setCollapsed(next) {
    applyCollapsed(next);
    try {
      await invoke('status_panel_set_collapsed', { collapsed });
    } catch (error) {
      elements.detail.textContent = String(error?.message || error);
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
    handle.addEventListener('mousedown', event => {
      if (event.button !== 0) return;
      // The chip is itself a button, so only nested controls block a drag.
      const control = event.target.closest('button');
      if (control && control !== handle) return;
      origin = { x: event.screenX, y: event.screenY };
      dragged = false;
    });
    window.addEventListener('mousemove', event => {
      if (!origin) return;
      if (!(event.buttons & 1)) return;
      if (Math.abs(event.screenX - origin.x) < DRAG_THRESHOLD &&
          Math.abs(event.screenY - origin.y) < DRAG_THRESHOLD) return;
      origin = null;
      dragged = true;
      // The window manager owns the pointer from here, so no mouseup arrives.
      void invoke('status_panel_start_drag').catch(() => {});
    });
    window.addEventListener('mouseup', () => {
      origin = null;
    });
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
      elements.detail.textContent = String(error?.message || error);
    }
  }

  function renderTelemetry(value) {
    telemetry = value || {};
    const channels = Array.isArray(telemetry.enabledChannels) ? telemetry.enabledChannels : [];
    elements.channels.textContent = channels.length ? channels.map(channel => `D${channel}`).join(', ') : '--';
    elements.logicRate.textContent = formatRate(telemetry.logicSampleRateHz || telemetry.sampleRateHz);
    elements.effectiveRate.textContent = formatRate(telemetry.pxlogicEffectiveSampleRateHz || telemetry.sampleRateHz);
    elements.mode.textContent = telemetry.pxlogicMode || '--';
    elements.threshold.textContent = Number.isFinite(telemetry.thresholdVolts) ? `${Number(telemetry.thresholdVolts).toFixed(3)} V` : '--';
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
  elements.collapse.addEventListener('click', () => setCollapsed(true));
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

  invoke('client_initial_state').then(initial => {
    hardware = initial.hardware;
    applyCollapsed(initial.settings?.statusPanel?.collapsed);
    elements.intro.hidden = Boolean(initial.settings?.guidance?.statusPanelIntroSeen);
    renderState(initial.bridgeState);
    renderTelemetry(initial.captureTelemetry);
  }).catch(error => {
    elements.detail.textContent = String(error?.message || error);
  });
}
