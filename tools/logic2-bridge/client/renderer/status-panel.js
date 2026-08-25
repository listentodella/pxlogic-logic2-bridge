'use strict';

const tauri = window.__TAURI__;
if (!tauri?.core?.invoke || !tauri?.event?.listen) {
  document.body.textContent = '状态面板需要由 PXLogic Bridge 启动';
} else {
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;
  const elements = {
    hide: document.querySelector('#hide-button'),
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
    const phase = state?.phase || 'stopped';
    elements.dot.className = `state-dot ${phase}`;
    elements.state.textContent = state?.message || '待机';
    elements.detail.textContent = state?.errorCode || (state?.actualPort ? 'Bridge 已连接到 Logic 2' : '等待 Logic 2 启动');
    elements.endpoint.textContent = state?.actualPort ? `WS ${state.actualPort}` : 'WebSocket --';
  }

  function renderTelemetry(telemetry) {
    const value = telemetry || {};
    const channels = Array.isArray(value.enabledChannels) ? value.enabledChannels : [];
    elements.channels.textContent = channels.length ? channels.map(channel => `D${channel}`).join(', ') : '--';
    elements.logicRate.textContent = formatRate(value.logicSampleRateHz || value.sampleRateHz);
    elements.effectiveRate.textContent = formatRate(value.pxlogicEffectiveSampleRateHz || value.sampleRateHz);
    elements.mode.textContent = value.pxlogicMode || '--';
    elements.threshold.textContent = Number.isFinite(value.thresholdVolts) ? `${Number(value.thresholdVolts).toFixed(3)} V` : '--';
    elements.usb.textContent = usbLabel(value.pxlogicUsbSpeed || selectedDevice()?.usbSpeed);
    const device = selectedDevice();
    elements.device.textContent = device?.profileModel || device?.product || device?.label || 'PXLogic';
    elements.serial.textContent = `序列号 ${device?.serialNumber || '--'}`;

    const underflows = Number(value.underflows || 0);
    const dropped = Number(value.droppedBytes || 0);
    const injected = Number(value.injectedBytes || 0);
    const quality = value.status === 'error' || dropped > 0 ? 'error' : underflows > 0 ? 'warn' : 'ok';
    elements.quality.className = quality;
    elements.quality.textContent = quality === 'error' ? '异常' : quality === 'warn' ? '需关注' : injected > 0 ? '正常' : '待统计';
    elements.fill.className = quality;
    elements.qualityDetail.textContent = injected > 0
      ? `已注入 ${formatBytes(injected)} · 下溢 ${underflows} 次 · 丢弃 ${formatBytes(dropped)}`
      : '开始采集后显示注入、下溢和丢弃统计';
  }

  elements.hide.addEventListener('click', () => invoke('status_panel_hide'));
  elements.main.addEventListener('click', () => invoke('main_window_show'));
  listen('bridge-state', event => renderState(event.payload));
  listen('capture-telemetry', event => renderTelemetry(event.payload));

  invoke('client_initial_state').then(initial => {
    hardware = initial.hardware;
    renderState(initial.bridgeState);
    renderTelemetry(initial.captureTelemetry);
  }).catch(error => {
    elements.detail.textContent = String(error?.message || error);
  });
}
