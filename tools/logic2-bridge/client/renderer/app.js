'use strict';

function createTauriApi() {
  const tauri = window.__TAURI__;
  if (!tauri?.core?.invoke || !tauri?.event?.listen) return null;
  const invoke = tauri.core.invoke;
  const listen = tauri.event.listen;
  return {
    initialState: () => invoke('client_initial_state'),
    saveSettings: settings => invoke('client_save_settings', { settings }),
    scanLogicApps: savedPath => invoke('logic_scan', { savedPath }),
    inspectLogicApp: appPath => invoke('logic_inspect', { appPath }),
    analyzeLogicApp: appPath => invoke('logic_analyze', { appPath }),
    browseLogicApp: () => invoke('logic_browse'),
    scanHardware: preferredDeviceId => invoke('pxlogic_scan', { preferredDeviceId }),
    start: settings => invoke('bridge_start', { settings }),
    restart: settings => invoke('bridge_restart', { settings }),
    stop: () => invoke('bridge_stop'),
    exportDiagnostics: () => invoke('diagnostics_export'),
    openLogs: () => invoke('logs_open'),
    openManual: () => invoke('manual_open'),
    onState: callback => void listen('bridge-state', event => callback(event.payload)),
    onTelemetry: callback => void listen('capture-telemetry', event => callback(event.payload)),
    onLog: callback => void listen('bridge-log', event => callback(event.payload)),
  };
}

const api = window.pxlogicBridge || createTauriApi();
if (!api) throw new Error('PXLogic Bridge desktop API is unavailable');
const elements = {
  status: document.querySelector('#status'),
  statusLabel: document.querySelector('#status-label'),
  logicSummary: document.querySelector('#logic-summary'),
  logicPath: document.querySelector('#logic-path'),
  logicApps: document.querySelector('#logic-apps'),
  logicVersion: document.querySelector('#logic-version'),
  graphFingerprint: document.querySelector('#graph-fingerprint'),
  compatibility: document.querySelector('#logic-compatibility'),
  compatibilityDetail: document.querySelector('#compatibility-detail'),
  compatibilityWarning: document.querySelector('#compatibility-warning'),
  compatibilityWarningMessage: document.querySelector('#compatibility-warning-message'),
  openManualButton: document.querySelector('#open-manual-button'),
  closeWarningButton: document.querySelector('#close-warning-button'),
  analyzeButton: document.querySelector('#analyze-button'),
  rescanButton: document.querySelector('#rescan-button'),
  browseButton: document.querySelector('#browse-button'),
  pxlogicSummary: document.querySelector('#pxlogic-summary'),
  pxlogicRescanButton: document.querySelector('#pxlogic-rescan-button'),
  pxlogicDevice: document.querySelector('#pxlogic-device'),
  pxlogicThreshold: document.querySelector('#pxlogic-threshold'),
  thresholdReference: document.querySelector('#threshold-reference'),
  thresholdVerified: document.querySelector('#threshold-verified'),
  thresholdGuidance: document.querySelector('#threshold-guidance'),
  pxlogicSerial: document.querySelector('#pxlogic-serial'),
  pxlogicUsbSpeed: document.querySelector('#pxlogic-usb-speed'),
  pxlogicFirmware: document.querySelector('#pxlogic-firmware'),
  pxlogicBitstream: document.querySelector('#pxlogic-bitstream'),
  pxlogicThresholdValue: document.querySelector('#pxlogic-threshold-value'),
  readinessSummary: document.querySelector('#readiness-summary'),
  logicReadiness: document.querySelector('#logic-readiness'),
  hardwareReadiness: document.querySelector('#hardware-readiness'),
  thresholdReadiness: document.querySelector('#threshold-readiness'),
  sessionSource: document.querySelector('#session-source'),
  sessionSourceDevice: document.querySelector('#session-source-device'),
  portAuto: document.querySelector('#port-auto'),
  portFixed: document.querySelector('#port-fixed'),
  preferredPort: document.querySelector('#preferred-port'),
  portSummary: document.querySelector('#port-summary'),
  screenQuadrant: document.querySelector('#screen-quadrant'),
  endpointLabel: document.querySelector('#endpoint-label'),
  captureSummary: document.querySelector('#capture-summary'),
  captureStatus: document.querySelector('#capture-status'),
  captureRate: document.querySelector('#capture-rate'),
  captureChannels: document.querySelector('#capture-channels'),
  captureThreshold: document.querySelector('#capture-threshold'),
  captureConverted: document.querySelector('#capture-converted'),
  captureInjected: document.querySelector('#capture-injected'),
  captureQueued: document.querySelector('#capture-queued'),
  captureLoss: document.querySelector('#capture-loss'),
  captureTrigger: document.querySelector('#capture-trigger'),
  logOutput: document.querySelector('#log-output'),
  recoveryPanel: document.querySelector('#recovery-panel'),
  recoveryTitle: document.querySelector('#recovery-title'),
  recoveryMessage: document.querySelector('#recovery-message'),
  recoveryCode: document.querySelector('#recovery-code'),
  exportDiagnosticsButton: document.querySelector('#export-diagnostics-button'),
  openLogsButton: document.querySelector('#open-logs-button'),
  footerMessage: document.querySelector('#footer-message'),
  startButton: document.querySelector('#start-button'),
};

let portMode = 'auto';
let currentState = { phase: 'stopped', actualPort: null, message: '待机' };
let currentInspection = null;
let currentHardware = null;
let currentTelemetry = { status: 'idle', enabledChannels: [] };
let thresholdProfiles = {};
let hardwareScanning = false;
let logicAnalyzing = false;
let inspectionSequence = 0;
const warnedCompatibilityFingerprints = new Set();

function isActive() {
  return ['starting', 'running', 'stopping', 'recovery'].includes(currentState.phase);
}

function needsRecovery() {
  return currentState.phase === 'recovery';
}

function selectedHardwareDevice() {
  const selectedId = currentHardware?.selectedDeviceId || elements.pxlogicDevice.value;
  return currentHardware?.devices?.find(device => device.id === selectedId) || null;
}

function formatUsbSpeed(speed) {
  const labels = {
    low: 'USB 1.x Low-Speed',
    full: 'USB 1.x Full-Speed',
    high: 'USB 2.0 High-Speed',
    super: 'USB 3.x SuperSpeed',
    'super-plus': 'USB 3.x SuperSpeed+',
  };
  return labels[String(speed || '').toLowerCase()] || speed || '--';
}

function isHardwareReady() {
  const device = selectedHardwareDevice();
  return Boolean(device?.ready && currentHardware?.firmwareResourceReady &&
    currentHardware?.bitstreamResourcesReady && !currentHardware?.error);
}

function isHardwareThresholdValid() {
  if (!elements.pxlogicThreshold.value.trim()) return false;
  const threshold = Number(elements.pxlogicThreshold.value);
  return Number.isFinite(threshold) && threshold >= 0 && threshold <= 6.668;
}

function setReadinessValue(element, label, tone) {
  element.textContent = label;
  element.className = `readiness-value readiness-${tone}`;
}

function selectedHardwareLabel() {
  const device = selectedHardwareDevice();
  if (!device) return 'PXLogic';
  const model = device.profileModel || device.product || device.label || 'PXLogic';
  return device.serialNumber ? `${model} · ${device.serialNumber}` : model;
}

function renderReadiness() {
  const logicReady = Boolean(currentInspection?.runnable);
  const hardwareReady = isHardwareReady();
  const thresholdReady = isHardwareThresholdValid();
  const threshold = Number(elements.pxlogicThreshold.value);

  if (currentInspection?.supported) {
    setReadinessValue(elements.logicReadiness, '正式支持', 'ok');
  } else if (currentInspection?.runnable) {
    setReadinessValue(elements.logicReadiness, '实验验证', 'warn');
  } else if (currentInspection?.path) {
    setReadinessValue(elements.logicReadiness, '不可用', 'error');
  } else {
    setReadinessValue(elements.logicReadiness, '待检测', 'muted');
  }

  if (hardwareScanning) {
    setReadinessValue(elements.hardwareReadiness, '检测中', 'muted');
  } else if (hardwareReady) {
    setReadinessValue(elements.hardwareReadiness, '已就绪', 'ok');
  } else if (selectedHardwareDevice()) {
    setReadinessValue(elements.hardwareReadiness, '设备异常', 'error');
  } else {
    setReadinessValue(elements.hardwareReadiness, '未连接', 'error');
  }

  if (thresholdReady) {
    const verified = elements.thresholdVerified.checked;
    setReadinessValue(
      elements.thresholdReadiness,
      `${threshold.toFixed(3)} V ${verified ? '已验证' : '待验证'}`,
      verified ? 'ok' : 'warn',
    );
  } else {
    setReadinessValue(elements.thresholdReadiness, '无效', 'error');
  }

  if (logicReady && hardwareReady && thresholdReady) {
    if (!elements.thresholdVerified.checked) {
      elements.readinessSummary.textContent = '可以启动采集；当前阈值仍需协议验证';
    } else {
      elements.readinessSummary.textContent = currentInspection.supported
        ? '正式支持路径已就绪'
        : '实验路径已就绪，尚未完成正式硬件验证';
    }
  } else {
    elements.readinessSummary.textContent = '完成异常项后才能启动 Bridge';
  }
  elements.sessionSourceDevice.textContent = selectedHardwareLabel();
  elements.sessionSource.classList.toggle('session-source-active', isActive());
}

function renderFooterMessage() {
  if (needsRecovery()) {
    elements.footerMessage.textContent =
      '当前采集已安全锁定；重新初始化会停止 Bridge 并重新准备 PXLogic';
    return;
  }
  if (currentState.phase === 'error') {
    elements.footerMessage.textContent = currentState.message;
    return;
  }
  if (isActive()) {
    elements.footerMessage.textContent =
      `Logic 2 请选择 Demo Logic Pro 16；波形数据来自 ${selectedHardwareLabel()}`;
    return;
  }
  if (currentInspection?.supported && isHardwareReady() && isHardwareThresholdValid()) {
    elements.footerMessage.textContent = '正式支持路径已就绪，可以启动 Logic 2';
  } else if (currentInspection?.runnable && isHardwareReady() && isHardwareThresholdValid()) {
    elements.footerMessage.textContent = '当前为实验验证路径，不属于正式支持';
  } else {
    elements.footerMessage.textContent = '完成启动检查后可以启动 Logic 2';
  }
}

function recoveryTitle(errorCode) {
  const titles = {
    PXLOGIC_RATE_MISMATCH: '采样率校验失败',
    PXLOGIC_CHANNEL_MISMATCH: '通道校验失败',
    PXLOGIC_CHANNEL_MAPPING_CHANGED: '通道映射发生变化',
    PXLOGIC_CONVERSION_FAILED: '数据转换失败',
    PXLOGIC_HELPER_START_FAILED: '采集进程启动失败',
    PXLOGIC_HELPER_EXITED: '采集进程异常退出',
    PXLOGIC_NOT_READY: 'PXLogic 尚未就绪',
    LOGIC_COMPATIBILITY: 'Logic 2 兼容性检查失败',
    BRIDGE_PROCESS_EXITED: 'Bridge 进程异常退出',
    BRIDGE_STATUS_FAILED: 'Bridge 状态读取失败',
    BRIDGE_START_FAILED: 'Bridge 启动失败',
  };
  return titles[errorCode] || 'Bridge 需要处理';
}

function renderRecoveryPanel() {
  const visible = ['error', 'recovery'].includes(currentState.phase) &&
    Boolean(currentState.errorCode || currentState.message);
  elements.recoveryPanel.hidden = !visible;
  if (!visible) return;
  elements.recoveryTitle.textContent = recoveryTitle(currentState.errorCode);
  elements.recoveryMessage.textContent = currentState.message;
  elements.recoveryCode.textContent = currentState.errorCode || 'BRIDGE_ERROR';
  elements.recoveryPanel.classList.toggle('recovery-required', needsRecovery());
}

function formatRate(rate) {
  if (!Number.isFinite(rate)) return '--';
  if (rate >= 1_000_000) return `${Number((rate / 1_000_000).toFixed(3))} MHz`;
  if (rate >= 1_000) return `${Number((rate / 1_000).toFixed(3))} kHz`;
  return `${rate} Hz`;
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) return '--';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = Math.max(0, bytes);
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const precision = value >= 100 || unit === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(precision)} ${units[unit]}`;
}

function renderTelemetry(telemetry) {
  currentTelemetry = telemetry || { status: 'idle', enabledChannels: [] };
  const statusLabels = {
    idle: '未采集',
    starting: '准备中',
    streaming: '采集中',
    stopped: '已停止',
    error: '异常',
  };
  const droppedKnown = Number.isFinite(currentTelemetry.droppedBytes);
  const underflowsKnown = Number.isFinite(currentTelemetry.underflows);
  const droppedBytes = droppedKnown ? currentTelemetry.droppedBytes : 0;
  const underflows = underflowsKnown ? currentTelemetry.underflows : 0;
  let quality = 'muted';
  let summary = '等待 Logic 2 开始采集';
  if (currentTelemetry.status === 'starting') {
    quality = 'warn';
    summary = '正在准备 PXLogic 采集链路';
  } else if (currentTelemetry.status === 'error') {
    quality = 'error';
    summary = '采集链路异常，请查看故障信息';
  } else if (droppedBytes > 0) {
    quality = 'error';
    summary = '检测到 native 注入数据丢弃';
  } else if (underflows > 0) {
    quality = 'warn';
    summary = '检测到 GraphServer 回调供数不足';
  } else if (['streaming', 'stopped'].includes(currentTelemetry.status) &&
             droppedKnown && underflowsKnown) {
    quality = 'ok';
    summary = currentTelemetry.status === 'streaming'
      ? '采集链路正常，尚未检测到丢弃或下溢'
      : '本次统计未检测到丢弃或下溢';
  } else if (currentTelemetry.status === 'streaming') {
    quality = 'ok';
    summary = 'PXLogic 正在转换数据，等待 native 注入统计';
  } else if (currentTelemetry.status === 'stopped') {
    summary = '采集已停止';
  }

  elements.captureSummary.textContent = summary;
  elements.captureStatus.textContent = statusLabels[currentTelemetry.status] || currentTelemetry.status;
  elements.captureStatus.className = `telemetry-value telemetry-${quality}`;
  elements.captureRate.textContent = formatRate(currentTelemetry.sampleRateHz);
  elements.captureChannels.textContent = currentTelemetry.enabledChannels?.length
    ? currentTelemetry.enabledChannels.map(channel => `D${channel}`).join(', ')
    : '--';
  elements.captureChannels.title = elements.captureChannels.textContent;
  elements.captureThreshold.textContent = Number.isFinite(currentTelemetry.thresholdVolts)
    ? `${currentTelemetry.thresholdVolts.toFixed(3)} V`
    : '--';
  elements.captureConverted.textContent = formatBytes(currentTelemetry.convertedBytes);
  elements.captureConverted.title = [
    Number.isFinite(currentTelemetry.crossChunks) ? `${currentTelemetry.crossChunks} chunks` : '',
    Number.isFinite(currentTelemetry.windowCount) ? `${currentTelemetry.windowCount} windows` : '',
    Number.isFinite(currentTelemetry.sampleCount) ? `${currentTelemetry.sampleCount} samples` : '',
  ].filter(Boolean).join(' · ');
  elements.captureInjected.textContent = formatBytes(currentTelemetry.injectedBytes);
  elements.captureInjected.title = Number.isFinite(currentTelemetry.callbackCount)
    ? `${currentTelemetry.callbackCount} callbacks`
    : '';
  elements.captureQueued.textContent = formatBytes(currentTelemetry.queuedBytes);
  elements.captureLoss.textContent = droppedKnown || underflowsKnown
    ? `${formatBytes(droppedBytes)} / ${underflows} 次`
    : '待统计';
  elements.captureLoss.className = `telemetry-value telemetry-${
    droppedBytes > 0 ? 'error' : underflows > 0 ? 'warn' : droppedKnown && underflowsKnown ? 'ok' : 'muted'
  }`;
  elements.captureTrigger.textContent = currentTelemetry.triggerDescription === 'off'
    ? '关闭'
    : currentTelemetry.triggerDescription
      ? `${currentTelemetry.triggerDescription}（GraphServer）`
      : '--';
}

function renderState(state) {
  currentState = state;
  elements.status.className = `status status-${state.phase}`;
  elements.statusLabel.textContent = state.message;
  elements.endpointLabel.textContent = state.actualPort
    ? `WebSocket 127.0.0.1:${state.actualPort}`
    : 'WebSocket --';
  elements.startButton.textContent = needsRecovery()
    ? '重新初始化 Bridge'
    : isActive()
      ? '停止 Bridge'
      : currentInspection?.supported
        ? '启动 Logic 2'
        : currentInspection?.runnable
          ? '启动实验验证'
          : '启动 Logic 2';
  elements.startButton.classList.toggle('stop', isActive() && !needsRecovery());
  const disableSettings = isActive();
  elements.logicPath.disabled = disableSettings || logicAnalyzing;
  elements.rescanButton.disabled = disableSettings || logicAnalyzing;
  elements.browseButton.disabled = disableSettings || logicAnalyzing;
  elements.analyzeButton.disabled = disableSettings || logicAnalyzing ||
    !elements.logicPath.value.trim();
  elements.pxlogicRescanButton.disabled = disableSettings || hardwareScanning;
  elements.pxlogicDevice.disabled = disableSettings || hardwareScanning ||
    !currentHardware?.devices?.length;
  elements.pxlogicThreshold.disabled = disableSettings;
  elements.thresholdReference.disabled = disableSettings;
  elements.thresholdVerified.disabled = disableSettings;
  elements.portAuto.disabled = disableSettings;
  elements.portFixed.disabled = disableSettings;
  elements.preferredPort.disabled = disableSettings || portMode !== 'fixed';
  elements.screenQuadrant.disabled = disableSettings;
  elements.startButton.disabled = state.phase === 'starting' || state.phase === 'stopping' ||
    logicAnalyzing ||
    !isHardwareThresholdValid() ||
    (!disableSettings && (!currentInspection?.runnable || !isHardwareReady()));
  renderReadiness();
  renderFooterMessage();
  renderRecoveryPanel();
}

function setPortMode(mode) {
  portMode = mode === 'fixed' ? 'fixed' : 'auto';
  elements.portAuto.classList.toggle('active', portMode === 'auto');
  elements.portFixed.classList.toggle('active', portMode === 'fixed');
  elements.preferredPort.disabled = isActive() || portMode !== 'fixed';
  elements.portSummary.textContent = portMode === 'auto'
    ? '端口由系统自动分配'
    : '占用时自动切换到可用端口';
}

function renderInspection(inspection) {
  currentInspection = inspection;
  const runtime = inspection?.nodeVersion ? ` · Node ${inspection.nodeVersion}` : '';
  elements.logicVersion.textContent = inspection?.version ? `${inspection.version}${runtime}` : '--';
  const graphIdentity = inspection?.graphIdentity || '';
  elements.graphFingerprint.textContent = graphIdentity || '--';
  elements.graphFingerprint.title = inspection?.graphSha256
    ? `${inspection.profileId || '未匹配 profile'}\n` +
      `${inspection.graphIdentityKind || 'unknown'} ${graphIdentity}\n` +
      `${inspection.graphPath || ''}\nsha256 ${inspection.graphSha256}`
    : '';
  elements.compatibility.className = 'compatibility';
  if (!inspection?.path) {
    elements.logicSummary.textContent = '未找到 Logic 2';
    elements.compatibility.textContent = '未检测';
    elements.compatibilityDetail.textContent = '请选择官方 Logic 2 安装后重新检查';
  } else if (inspection.supported) {
    elements.logicSummary.textContent = `已匹配 ${inspection.profileId}`;
    elements.compatibility.textContent = '正式支持';
    elements.compatibility.classList.add('compatible');
    elements.compatibilityDetail.textContent =
      '该 GraphServer 已完成真实 PXLogic 采集验证';
  } else if (inspection.runnable) {
    elements.logicSummary.textContent = inspection.error || `实验性 ${inspection.profileId}`;
    elements.compatibility.textContent = '实验验证';
    elements.compatibility.classList.add('experimental');
    elements.compatibilityDetail.textContent = inspection.hookStatus === 'candidate'
      ? '自动分析得到唯一候选，但 ABI 与真实采集尚未验证'
      : '仅用于真实硬件验证，尚未达到正式支持标准';
  } else {
    elements.logicSummary.textContent = inspection.error || '安装不可用';
    elements.compatibility.textContent = inspection.hookStatus === 'unsupported'
      ? '不可用'
      : inspection.hookStatus === 'unknown'
        ? '未收录'
        : inspection.graphIdentity && inspection.hookStatus
          ? '待验证'
          : '不兼容';
    elements.compatibility.classList.add('incompatible');
    elements.compatibilityDetail.textContent = inspection.hookStatus === 'unsupported'
      ? '没有得到可安全注入的唯一候选，Bridge 不会修改该 GraphServer'
      : '该安装尚未满足 Bridge 的安全启动条件';
  }
  renderState(currentState);
  showCompatibilityWarning(inspection);
}

function showCompatibilityWarning(inspection) {
  if (inspection?.hookStatus !== 'unsupported') return;
  const key = inspection.graphSha256 || `${inspection.path}:${inspection.error}`;
  if (warnedCompatibilityFingerprints.has(key)) return;
  warnedCompatibilityFingerprints.add(key);
  elements.compatibilityWarningMessage.textContent = inspection.error ||
    '自动分析没有得到唯一且可安全注入的候选。';
  if (!elements.compatibilityWarning.open) elements.compatibilityWarning.showModal();
}

function renderApplications(applications) {
  elements.logicApps.replaceChildren();
  for (const application of applications) {
    const option = document.createElement('option');
    option.value = application.path;
    option.label = application.version ? `Logic ${application.version}` : 'Logic';
    elements.logicApps.append(option);
  }
}

function renderHardware(hardware) {
  currentHardware = hardware;
  elements.pxlogicDevice.replaceChildren();
  for (const device of hardware?.devices || []) {
    const option = document.createElement('option');
    option.value = device.id;
    const model = device.profileModel || device.product || device.label || 'PXLogic';
    option.textContent = device.serialNumber ? `${model} · ${device.serialNumber}` : model;
    elements.pxlogicDevice.append(option);
  }
  if (!hardware?.devices?.length) {
    const option = document.createElement('option');
    option.value = '';
    option.textContent = '未检测到设备';
    elements.pxlogicDevice.append(option);
  }
  elements.pxlogicDevice.value = hardware?.selectedDeviceId || '';
  const device = selectedHardwareDevice();
  if (hardware?.error) {
    elements.pxlogicSummary.textContent = hardware.error;
  } else if (!device) {
    elements.pxlogicSummary.textContent = '未检测到 PXLogic';
  } else if (device.ready) {
    elements.pxlogicSummary.textContent = device.profileModel || device.label || 'PXLogic 已就绪';
  } else {
    elements.pxlogicSummary.textContent = device.probeError || '设备尚未就绪';
  }
  elements.pxlogicSerial.textContent = device?.serialNumber || '--';
  elements.pxlogicSerial.title = device?.serialNumber || '';
  elements.pxlogicUsbSpeed.textContent = formatUsbSpeed(device?.usbSpeed);
  elements.pxlogicUsbSpeed.title = formatUsbSpeed(device?.usbSpeed);
  elements.pxlogicFirmware.textContent = hardware?.firmwareResourceReady
    ? device?.ready ? '就绪' : '资源就绪'
    : '缺失';
  elements.pxlogicBitstream.textContent = hardware?.bitstreamResourcesReady ? '自动加载' : '缺失';
  elements.pxlogicFirmware.classList.toggle(
    'hardware-error',
    !hardware?.firmwareResourceReady || Boolean(device && !device.ready),
  );
  elements.pxlogicBitstream.classList.toggle(
    'hardware-error',
    !hardware?.bitstreamResourcesReady,
  );
  updateThresholdLabel();
  renderState(currentState);
}

function rememberThresholdProfile(deviceId = elements.pxlogicDevice.value) {
  if (!deviceId || !isHardwareThresholdValid()) return;
  thresholdProfiles[deviceId] = {
    volts: Number(elements.pxlogicThreshold.value),
    verified: elements.thresholdVerified.checked,
    reference: elements.thresholdReference.value,
  };
}

function applyThresholdProfile(deviceId) {
  const profile = thresholdProfiles[deviceId];
  if (profile && Number.isFinite(Number(profile.volts))) {
    elements.pxlogicThreshold.value = String(profile.volts);
    elements.thresholdVerified.checked = profile.verified === true;
    elements.thresholdReference.value = elements.thresholdReference.querySelector(
      `option[value="${CSS.escape(profile.reference || 'custom')}"]`,
    ) ? profile.reference : 'custom';
  } else {
    elements.thresholdVerified.checked = false;
    elements.thresholdReference.value = 'custom';
  }
  updateThresholdLabel();
}

function renderThresholdGuidance() {
  if (elements.thresholdVerified.checked) {
    elements.thresholdGuidance.textContent =
      `已为 ${selectedHardwareLabel()} 记录协议验证结果`;
    elements.thresholdGuidance.className = 'threshold-guidance threshold-guidance-verified';
    return;
  }
  const reference = elements.thresholdReference.value;
  if (reference === 'fixture-stm32-spi') {
    elements.thresholdGuidance.textContent =
      '2.2 V 只在现有 STM32 3.3 V SPI 夹具验证；当前目标仍需复核';
  } else if (reference.startsWith('logic-')) {
    elements.thresholdGuidance.textContent =
      '电平中点仅是起始值；请用已知协议内容确认数字判定正确';
  } else {
    elements.thresholdGuidance.textContent =
      '检测到边沿不代表数据正确，当前阈值尚未验证';
  }
  elements.thresholdGuidance.className = 'threshold-guidance';
}

function updateThresholdLabel() {
  const threshold = Number(elements.pxlogicThreshold.value);
  const valid = isHardwareThresholdValid();
  const verified = valid && elements.thresholdVerified.checked;
  elements.pxlogicThresholdValue.textContent = valid
    ? `${threshold.toFixed(3)} V${verified ? ' · 已验证' : ''}`
    : '无效';
  elements.pxlogicThresholdValue.title = elements.pxlogicThresholdValue.textContent;
  elements.pxlogicThresholdValue.classList.toggle('hardware-error', !valid);
  elements.pxlogicThreshold.setCustomValidity(
    valid ? '' : '电压判断阈值必须在 0.000 V 到 6.668 V 之间',
  );
  renderThresholdGuidance();
  renderReadiness();
  renderFooterMessage();
}

async function inspectPath() {
  const sequence = ++inspectionSequence;
  const inspection = await api.inspectLogicApp(elements.logicPath.value.trim());
  if (sequence === inspectionSequence) renderInspection(inspection);
}

function readSettings() {
  const windowPosition = elements.screenQuadrant.value;
  rememberThresholdProfile();
  return {
    logicAppPath: elements.logicPath.value.trim(),
    portMode,
    preferredPort: Number(elements.preferredPort.value),
    screenQuadrant: windowPosition === 'maximized' ? 3 : Number(windowPosition),
    maximizeLogicWindow: windowPosition === 'maximized',
    pxlogicDeviceId: elements.pxlogicDevice.value,
    pxlogicThresholdVolts: Number(elements.pxlogicThreshold.value),
    pxlogicThresholdProfiles: JSON.parse(JSON.stringify(thresholdProfiles)),
  };
}

function persistSettings() {
  void api.saveSettings(readSettings()).catch(error => appendLog(`[client] ${errorMessage(error)}`));
}

function appendLog(line) {
  const existing = elements.logOutput.textContent === 'PXLogic Bridge 已就绪'
    ? ''
    : `${elements.logOutput.textContent}\n`;
  const lines = `${existing}${line}`.split('\n').slice(-120);
  elements.logOutput.textContent = lines.join('\n');
  elements.logOutput.scrollTop = elements.logOutput.scrollHeight;
}

function errorMessage(error) {
  return error?.message || String(error);
}

elements.logicPath.addEventListener('change', () => {
  void inspectPath();
  persistSettings();
});
elements.logicPath.addEventListener('blur', inspectPath);
elements.portAuto.addEventListener('click', () => {
  setPortMode('auto');
  persistSettings();
});
elements.portFixed.addEventListener('click', () => {
  setPortMode('fixed');
  persistSettings();
});
elements.preferredPort.addEventListener('change', persistSettings);
elements.screenQuadrant.addEventListener('change', persistSettings);
elements.rescanButton.addEventListener('click', async () => {
  elements.rescanButton.disabled = true;
  const applications = await api.scanLogicApps(elements.logicPath.value.trim());
  renderApplications(applications);
  const preferred = applications.find(item => item.runnable) || applications[0];
  if (preferred) {
    elements.logicPath.value = preferred.path;
    renderInspection(preferred);
    persistSettings();
  } else {
    renderInspection(null);
  }
  elements.rescanButton.disabled = false;
});
elements.analyzeButton.addEventListener('click', async () => {
  if (typeof api.analyzeLogicApp !== 'function') return;
  logicAnalyzing = true;
  elements.logicSummary.textContent = '正在本机分析 GraphServer...';
  renderState(currentState);
  try {
    const inspection = await api.analyzeLogicApp(elements.logicPath.value.trim());
    renderInspection(inspection);
    if (!inspection.runnable && inspection.error) {
      appendLog(`[compatibility] ${inspection.error}`);
    }
  } catch (error) {
    appendLog(`[compatibility] ${errorMessage(error)}`);
  } finally {
    logicAnalyzing = false;
    renderState(currentState);
  }
});
elements.browseButton.addEventListener('click', async () => {
  const inspection = await api.browseLogicApp();
  if (!inspection) return;
  elements.logicPath.value = inspection.path;
  renderInspection(inspection);
  persistSettings();
});
elements.pxlogicDevice.addEventListener('change', () => {
  const previousDeviceId = currentHardware?.selectedDeviceId;
  rememberThresholdProfile(previousDeviceId);
  if (currentHardware) currentHardware.selectedDeviceId = elements.pxlogicDevice.value;
  applyThresholdProfile(elements.pxlogicDevice.value);
  renderHardware(currentHardware);
  persistSettings();
});
elements.pxlogicThreshold.addEventListener('input', () => {
  elements.thresholdReference.value = 'custom';
  elements.thresholdVerified.checked = false;
  updateThresholdLabel();
  renderState(currentState);
});
elements.pxlogicThreshold.addEventListener('change', () => {
  if (!elements.pxlogicThreshold.reportValidity()) return;
  rememberThresholdProfile();
  persistSettings();
});
elements.thresholdReference.addEventListener('change', () => {
  const option = elements.thresholdReference.selectedOptions[0];
  const volts = Number(option?.dataset.volts);
  if (Number.isFinite(volts)) elements.pxlogicThreshold.value = String(volts);
  elements.thresholdVerified.checked = false;
  updateThresholdLabel();
  rememberThresholdProfile();
  persistSettings();
});
elements.thresholdVerified.addEventListener('change', () => {
  updateThresholdLabel();
  rememberThresholdProfile();
  persistSettings();
});
elements.pxlogicRescanButton.addEventListener('click', async () => {
  hardwareScanning = true;
  renderState(currentState);
  try {
    const hardware = await api.scanHardware(elements.pxlogicDevice.value);
    renderHardware(hardware);
    persistSettings();
  } catch (error) {
    appendLog(`[client] ${errorMessage(error)}`);
  } finally {
    hardwareScanning = false;
    renderState(currentState);
  }
});
elements.openLogsButton.addEventListener('click', () => api.openLogs());
elements.exportDiagnosticsButton.addEventListener('click', async () => {
  try {
    if (typeof api.exportDiagnostics !== 'function') {
      throw new Error('当前客户端不支持诊断导出');
    }
    const path = await api.exportDiagnostics();
    if (path) appendLog(`[client] 诊断已导出: ${path}`);
  } catch (error) {
    appendLog(`[client] ${errorMessage(error)}`);
  }
});
elements.openManualButton.addEventListener('click', async () => {
  try {
    if (typeof api.openManual !== 'function') throw new Error('当前客户端未包含手工指南入口');
    await api.openManual();
  } catch (error) {
    appendLog(`[compatibility] ${errorMessage(error)}`);
  }
});
elements.closeWarningButton.addEventListener('click', () => {
  elements.compatibilityWarning.close();
});
elements.startButton.addEventListener('click', async () => {
  try {
    if (isActive()) {
      if (needsRecovery()) {
        if (typeof api.restart !== 'function') throw new Error('当前客户端不支持安全重新初始化');
        const state = await api.restart(readSettings());
        renderState(state);
      } else {
        await api.stop();
      }
    } else {
      if (!elements.pxlogicThreshold.reportValidity()) return;
      const state = await api.start(readSettings());
      renderState(state);
    }
  } catch (error) {
    const message = errorMessage(error);
    appendLog(`[client] ${message}`);
    elements.footerMessage.textContent = message;
  }
});

api.onState(renderState);
if (typeof api.onTelemetry === 'function') api.onTelemetry(renderTelemetry);
api.onLog(appendLog);

async function initialize() {
  if (typeof api.analyzeLogicApp !== 'function') elements.analyzeButton.hidden = true;
  if (typeof api.exportDiagnostics !== 'function') elements.exportDiagnosticsButton.hidden = true;
  const initial = await api.initialState();
  renderApplications(initial.applications);
  elements.logicPath.value = initial.settings.logicAppPath;
  elements.preferredPort.value = initial.settings.preferredPort;
  elements.pxlogicThreshold.value = String(
    initial.settings.pxlogicThresholdVolts ??
      initial.settings.pxlogicComparatorThresholdVolts ?? 1.8,
  );
  thresholdProfiles = JSON.parse(JSON.stringify(initial.settings.pxlogicThresholdProfiles || {}));
  elements.screenQuadrant.value = initial.settings.maximizeLogicWindow === false
    ? String(initial.settings.screenQuadrant)
    : 'maximized';
  setPortMode(initial.settings.portMode);
  renderHardware(initial.hardware);
  applyThresholdProfile(initial.hardware?.selectedDeviceId);
  renderTelemetry(initial.captureTelemetry);
  if (initial.logs.length) {
    elements.logOutput.textContent = initial.logs.slice(-120).join('\n');
  }
  const selected = initial.applications.find(item => item.path === initial.settings.logicAppPath);
  renderInspection(selected || await api.inspectLogicApp(initial.settings.logicAppPath));
  renderState(initial.bridgeState);
}

initialize().catch(error => {
  appendLog(`[client] ${errorMessage(error)}`);
  renderInspection(null);
});
