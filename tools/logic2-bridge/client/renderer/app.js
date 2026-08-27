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
    showStatusPanel: () => invoke('status_panel_show'),
    hideStatusPanel: () => invoke('status_panel_hide'),
    toggleStatusPanel: () => invoke('status_panel_toggle'),
    completeOnboarding: () => invoke('onboarding_complete'),
    runningLogicInstances: () => invoke('logic_running_instances'),
    closeLogicInstances: pids => invoke('logic_close_instances', { pids }),
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
  experimentalConfirmation: document.querySelector('#experimental-confirmation'),
  experimentalConfirmationMessage: document.querySelector('#experimental-confirmation-message'),
  experimentalConfirmationCheckbox: document.querySelector('#experimental-confirmation-checkbox'),
  continueExperimentalButton: document.querySelector('#continue-experimental-button'),
  cancelExperimentalButton: document.querySelector('#cancel-experimental-button'),
  openManualButton: document.querySelector('#open-manual-button'),
  closeWarningButton: document.querySelector('#close-warning-button'),
  analyzeButton: document.querySelector('#analyze-button'),
  rescanButton: document.querySelector('#rescan-button'),
  browseButton: document.querySelector('#browse-button'),
  pxlogicSummary: document.querySelector('#pxlogic-summary'),
  pxlogicRescanButton: document.querySelector('#pxlogic-rescan-button'),
  pxlogicDevice: document.querySelector('#pxlogic-device'),
  pxlogicThreshold: document.querySelector('#pxlogic-threshold'),
  pxlogicFirmwareVersion: document.querySelector('#pxlogic-firmware-version'),
  firmwareDowngradeWarning: document.querySelector('#firmware-downgrade-warning'),
  firmwareDowngradeConfirmation: document.querySelector('#firmware-downgrade-confirmation'),
  firmwareDowngradeMessage: document.querySelector('#firmware-downgrade-message'),
  firmwareDowngradeCheckbox: document.querySelector('#firmware-downgrade-checkbox'),
  confirmFirmwareDowngradeButton: document.querySelector('#confirm-firmware-downgrade-button'),
  cancelFirmwareDowngradeButton: document.querySelector('#cancel-firmware-downgrade-button'),
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
  captureMapping: document.querySelector('#capture-mapping'),
  logOutput: document.querySelector('#log-output'),
  recoveryPanel: document.querySelector('#recovery-panel'),
  recoveryTitle: document.querySelector('#recovery-title'),
  recoveryMessage: document.querySelector('#recovery-message'),
  recoveryCode: document.querySelector('#recovery-code'),
  exportDiagnosticsButton: document.querySelector('#export-diagnostics-button'),
  openLogsButton: document.querySelector('#open-logs-button'),
  footerMessage: document.querySelector('#footer-message'),
  startButton: document.querySelector('#start-button'),
  statusPanelButton: document.querySelector('#status-panel-button'),
  onboardingButton: document.querySelector('#onboarding-button'),
  wizard: document.querySelector('#onboarding-wizard'),
  wizardStepIndex: document.querySelector('#wizard-step-index'),
  wizardSteps: Array.from(document.querySelectorAll('.wizard-step')),
  wizardSkip: document.querySelector('#wizard-skip'),
  wizardBack: document.querySelector('#wizard-back'),
  wizardNext: document.querySelector('#wizard-next'),
  wizardLogicPath: document.querySelector('#wizard-logic-path'),
  wizardLogicVerdict: document.querySelector('#wizard-logic-verdict'),
  wizardDevice: document.querySelector('#wizard-device'),
  wizardFirmware: document.querySelector('#wizard-firmware'),
  wizardThreshold: document.querySelector('#wizard-threshold'),
  wizardThresholdVerified: document.querySelector('#wizard-threshold-verified'),
  readinessItems: Array.from(document.querySelectorAll('.readiness-item')),
  logicRunningConfirmation: document.querySelector('#logic-running-confirmation'),
  logicRunningMessage: document.querySelector('#logic-running-message'),
  logicRunningCheckbox: document.querySelector('#logic-running-checkbox'),
  cancelLogicRunningButton: document.querySelector('#cancel-logic-running-button'),
  confirmLogicRunningButton: document.querySelector('#confirm-logic-running-button'),
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
let experimentalConfirmationToken = null;
let wizardStep = 1;

/// Scrolls the control that resolves a readiness item into view and flashes its
/// section, so a guidance affordance always lands somewhere actionable.
function focusSetting(targetId) {
  const target = document.getElementById(targetId);
  if (!target) return;
  const section = target.closest('.settings-section');
  (section || target).scrollIntoView({ behavior: 'smooth', block: 'center' });
  if (section) {
    section.classList.remove('section-highlight');
    // Reading a layout property restarts the animation when the same section is
    // requested twice in a row.
    void section.offsetWidth;
    section.classList.add('section-highlight');
    setTimeout(() => section.classList.remove('section-highlight'), 1600);
  }
  target.focus({ preventScroll: true });
}

function renderWizardStep() {
  const total = elements.wizardSteps.length;
  wizardStep = Math.min(Math.max(wizardStep, 1), total);
  for (const step of elements.wizardSteps) {
    step.hidden = Number(step.dataset.step) !== wizardStep;
  }
  elements.wizardStepIndex.textContent = String(wizardStep);
  elements.wizardBack.disabled = wizardStep === 1;
  elements.wizardNext.textContent = wizardStep === total ? '开始使用' : '下一步';
}

// The wizard never duplicates a control; it reads the same live values the
// settings sections render so the two can never disagree.
function renderWizardReadouts() {
  elements.wizardLogicPath.textContent = elements.logicPath.value.trim() || '尚未选择';
  elements.wizardLogicPath.title = elements.wizardLogicPath.textContent;
  elements.wizardLogicVerdict.textContent = elements.compatibility.textContent || '待检测';
  elements.wizardDevice.textContent = selectedHardwareDevice()
    ? selectedHardwareLabel()
    : '未检测到 PXLogic';
  elements.wizardDevice.title = elements.wizardDevice.textContent;
  const release = findFirmwareRelease(elements.pxlogicFirmwareVersion.value);
  elements.wizardFirmware.textContent = release
    ? `${release.label} · ${release.firmwareVersion}${release.latest ? '（最新）' : ''}`
    : '--';
  const threshold = Number(elements.pxlogicThreshold.value);
  elements.wizardThreshold.textContent = Number.isFinite(threshold)
    ? `${threshold.toFixed(3)} V`
    : '--';
  elements.wizardThresholdVerified.textContent = elements.thresholdVerified.checked
    ? '已用已知协议验证'
    : '尚未验证';
}

function openWizard(step = 1) {
  wizardStep = step;
  renderWizardStep();
  renderWizardReadouts();
  if (!elements.wizard.open) elements.wizard.showModal();
}

async function recordOnboardingComplete() {
  if (typeof api.completeOnboarding !== 'function') return;
  try {
    await api.completeOnboarding();
  } catch (error) {
    appendLog(`[client] 无法记录引导状态：${errorMessage(error)}`);
  }
}

function finishWizard() {
  if (elements.wizard.open) elements.wizard.close();
}

/*
 * Resolves a Logic window the Bridge would collide with.
 *
 * A running window cannot be handed to a new Bridge session: Logic reconnects to
 * the address it was given but never rebuilds its device state in the fresh
 * GraphServer, so the window looks connected while capture silently does nothing.
 * Replacing it is the only reliable option, and closing someone's window can lose
 * unsaved captures, so it only ever happens through this confirmation.
 */
async function resolveRunningLogic() {
  if (typeof api.runningLogicInstances !== 'function') return true;
  let instances;
  try {
    instances = await api.runningLogicInstances();
  } catch (error) {
    appendLog(`[client] 无法检查已运行的 Logic 2：${errorMessage(error)}`);
    return true;
  }
  const blocking = instances || [];
  if (!blocking.length) return true;

  const pids = blocking.map(instance => instance.pid);
  const described = blocking
    .map(instance => (instance.graphPort ? `${instance.pid}（Bridge 启动）` : `${instance.pid}`))
    .join('、');
  elements.logicRunningMessage.textContent =
    `检测到 ${pids.length} 个 Logic 2 窗口正在运行（pid ${described}）。`;
  elements.logicRunningCheckbox.checked = false;
  elements.confirmLogicRunningButton.disabled = true;
  const accepted = await new Promise(resolve => {
    const settle = value => {
      elements.cancelLogicRunningButton.removeEventListener('click', onCancel);
      elements.confirmLogicRunningButton.removeEventListener('click', onConfirm);
      elements.logicRunningConfirmation.removeEventListener('close', onClose);
      if (elements.logicRunningConfirmation.open) elements.logicRunningConfirmation.close();
      resolve(value);
    };
    const onCancel = () => settle(false);
    const onConfirm = () => {
      if (!elements.logicRunningCheckbox.checked) return;
      settle(true);
    };
    // Escape closes a <dialog> natively, which must count as declining.
    const onClose = () => settle(false);
    elements.cancelLogicRunningButton.addEventListener('click', onCancel);
    elements.confirmLogicRunningButton.addEventListener('click', onConfirm);
    elements.logicRunningConfirmation.addEventListener('close', onClose);
    elements.logicRunningConfirmation.showModal();
  });
  if (!accepted) {
    elements.footerMessage.textContent = '已取消启动：请先保存并关闭正在运行的 Logic 2';
    return false;
  }
  try {
    await api.closeLogicInstances(pids);
    appendLog(`[client] 已关闭 Logic 2（pid ${pids.join(', ')}）`);
  } catch (error) {
    const message = errorMessage(error);
    appendLog(`[client] ${message}`);
    elements.footerMessage.textContent = message;
    return false;
  }
  return true;
}

function isActive() {
  return ['starting', 'running', 'stopping', 'recovery'].includes(currentState.phase);
}

function needsRecovery() {
  return currentState.phase === 'recovery';
}

function needsUsbReconnectRecovery() {
  return needsRecovery() && currentState.recoveryAction === 'rescan-and-restart';
}

function needsExperimentalConfirmation() {
  return Boolean(currentInspection?.runnable && !currentInspection.supported);
}

function clearExperimentalConfirmation() {
  experimentalConfirmationToken = null;
}

function requestExperimentalConfirmation() {
  if (!needsExperimentalConfirmation()) return true;
  if (experimentalConfirmationToken &&
      experimentalConfirmationToken.confirmed &&
      experimentalConfirmationToken.phase === currentState.phase &&
      experimentalConfirmationToken.graphSha256 === currentInspection.graphSha256 &&
      experimentalConfirmationToken.profileId === currentInspection.profileId) {
    return true;
  }
  experimentalConfirmationToken = {
    profileId: currentInspection.profileId || '',
    graphSha256: currentInspection.graphSha256 || '',
    phase: currentState.phase,
    confirmed: false,
  };
  const profile = currentInspection.profileId || '当前 profile';
  elements.experimentalConfirmationMessage.textContent =
    `${profile} 尚未完成真实 ABI 与 PXLogic 采集验证。继续会以实验性 profile 启动，` +
    '可能导致 Logic 2 无法启动或需要重新初始化；不会修改官方 profile。';
  elements.experimentalConfirmationCheckbox.checked = false;
  elements.continueExperimentalButton.disabled = true;
  if (!elements.experimentalConfirmation.open) elements.experimentalConfirmation.showModal();
  return false;
}

function consumeExperimentalConfirmationFingerprint() {
  const fingerprint = needsExperimentalConfirmation() &&
    experimentalConfirmationToken?.confirmed === true &&
    experimentalConfirmationToken?.phase === currentState.phase &&
    experimentalConfirmationToken?.graphSha256 === currentInspection.graphSha256 &&
    experimentalConfirmationToken?.profileId === (currentInspection.profileId || '')
    ? experimentalConfirmationToken.graphSha256
    : null;
  clearExperimentalConfirmation();
  return fingerprint;
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
    elements.footerMessage.textContent = needsUsbReconnectRecovery()
      ? 'USB 设备已重新连接，采集已安全停止；重新扫描并初始化后即可继续'
      : '当前采集已安全锁定；重新初始化会停止 Bridge 并重新准备 PXLogic';
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
    PXLOGIC_USB_REENUMERATED: 'USB 设备已重新连接',
    GRAPH_ANALYZER_CLEANUP_CRASH: 'Logic 2 分析器清理异常',
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

function resolvePxviewStreamMode(device, channelCount, sampleRateHz) {
  const speed = String(device?.usbSpeed || '').toLowerCase();
  const modes = speed === 'super'
    ? [
      ['STREAM_LOGIC50x32', 32, 50_000_000],
      ['STREAM_LOGIC125x16', 16, 125_000_000],
      ['STREAM_LOGIC250x8', 8, 250_000_000],
      ['STREAM_LOGIC500x4', 4, 500_000_000],
      ['STREAM_LOGIC1000x2', 2, 1_000_000_000],
    ]
    : [
      ['STREAM_LOGIC200x1', 1, 200_000_000],
      ['STREAM_LOGIC100x2', 2, 100_000_000],
      ['STREAM_LOGIC50x4', 4, 50_000_000],
      ['STREAM_LOGIC25x8', 8, 25_000_000],
      ['STREAM_LOGIC10x16', 16, 10_000_000],
      ['STREAM_LOGIC5x32', 32, 5_000_000],
    ];
  const selected = modes
    .filter(([, channels, maxRate]) => channels >= channelCount && sampleRateHz <= maxRate)
    .sort((left, right) => left[1] - right[1] || left[2] - right[2])[0];
  return selected ? `${selected[0]} · ${formatRate(sampleRateHz)}` : '当前通道/采样率组合超出 PXView Stream 能力';
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
  const device = selectedHardwareDevice();
  const channels = currentTelemetry.enabledChannels?.length || 0;
  const rate = Number(currentTelemetry.sampleRateHz);
  elements.captureMapping.textContent = channels > 0 && Number.isFinite(rate)
    ? `Logic ${formatRate(rate)} / ${channels} lanes -> ${resolvePxviewStreamMode(device, channels, rate)}`
    : '等待 Logic 2 下发通道和采样率';
}

function renderState(state) {
  currentState = state;
  if (experimentalConfirmationToken && experimentalConfirmationToken.phase !== state.phase) {
    clearExperimentalConfirmation();
    if (elements.experimentalConfirmation.open) elements.experimentalConfirmation.close();
  }
  elements.status.className = `status status-${state.phase}`;
  elements.statusLabel.textContent = state.message;
  elements.endpointLabel.textContent = state.actualPort
    ? `WebSocket 127.0.0.1:${state.actualPort}`
    : 'WebSocket --';
  if (needsUsbReconnectRecovery()) {
    elements.startButton.textContent = '重新扫描并初始化';
  } else if (needsRecovery()) {
    elements.startButton.textContent = '重新初始化 Bridge';
  } else if (isActive()) {
    elements.startButton.textContent = '停止 Bridge';
  } else if (currentInspection?.runnable && !currentInspection.supported) {
    elements.startButton.textContent = '启动实验验证';
  } else {
    elements.startButton.textContent = '启动 Logic 2';
  }
  elements.startButton.classList.toggle('stop', isActive() && !needsRecovery());
  const disableSettings = isActive();
  elements.logicPath.disabled = disableSettings || logicAnalyzing;
  elements.rescanButton.disabled = disableSettings || logicAnalyzing;
  elements.browseButton.disabled = disableSettings || logicAnalyzing;
  elements.analyzeButton.disabled = disableSettings || logicAnalyzing ||
    !elements.logicPath.value.trim();
  const disableHardwareSelection = disableSettings && !needsUsbReconnectRecovery();
  elements.pxlogicRescanButton.disabled = disableHardwareSelection || hardwareScanning;
  elements.pxlogicDevice.disabled = disableHardwareSelection || hardwareScanning ||
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
  if (experimentalConfirmationToken &&
      (experimentalConfirmationToken.graphSha256 !== inspection?.graphSha256 ||
       experimentalConfirmationToken.profileId !== (inspection?.profileId || ''))) {
    clearExperimentalConfirmation();
    if (elements.experimentalConfirmation.open) elements.experimentalConfirmation.close();
  }
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
  // Name the image that would actually be programmed, so a non-default
  // selection is visible without opening the picker.
  elements.pxlogicFirmware.title = hardware?.firmwareRelease
    ? `${hardware.firmwareRelease.label} · ${hardware.firmwareRelease.firmwareVersion}`
    : '';
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

function readSettings(pendingProfileFingerprint = null) {
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
    pxlogicFirmwareId: elements.pxlogicFirmwareVersion.value,
    pendingProfileFingerprint,
  };
}

function persistSettings() {
  void api.saveSettings(readSettings()).catch(error => appendLog(`[client] ${errorMessage(error)}`));
}

// Firmware images offered by resources/firmware/releases.json, newest first.
let firmwareReleases = [];
// The selection to fall back to when the user cancels a downgrade, so a
// cancelled dialog cannot leave a non-latest image selected.
let lastConfirmedFirmwareId = '';

function latestFirmwareRelease() {
  return firmwareReleases.find(release => release.latest) || firmwareReleases[0] || null;
}

function findFirmwareRelease(id) {
  return firmwareReleases.find(release => release.id === id) || null;
}

function renderFirmwareReleases(releases, selectedId) {
  firmwareReleases = Array.isArray(releases) ? releases : [];
  const select = elements.pxlogicFirmwareVersion;
  select.replaceChildren();
  if (!firmwareReleases.length) {
    // Nothing to choose from: keep the control out of the way rather than
    // showing an empty picker the Bridge cannot honour.
    select.disabled = true;
    select.parentElement.hidden = true;
    renderFirmwareWarning();
    return;
  }
  select.parentElement.hidden = false;
  select.disabled = false;
  for (const release of firmwareReleases) {
    const option = document.createElement('option');
    option.value = release.id;
    option.textContent = release.latest
      ? `${release.label} · ${release.firmwareVersion}（最新）`
      : `${release.label} · ${release.firmwareVersion}`;
    select.append(option);
  }
  const latest = latestFirmwareRelease();
  const resolved = findFirmwareRelease(selectedId) || latest;
  select.value = resolved ? resolved.id : '';
  lastConfirmedFirmwareId = select.value;
  renderFirmwareWarning();
}

function renderFirmwareWarning() {
  const warning = elements.firmwareDowngradeWarning;
  const release = findFirmwareRelease(elements.pxlogicFirmwareVersion.value);
  if (!release || release.latest) {
    warning.hidden = true;
    warning.textContent = '';
    return;
  }
  const latest = latestFirmwareRelease();
  const suffix = latest ? `，最新版本为 ${latest.label} · ${latest.firmwareVersion}` : '';
  warning.hidden = false;
  warning.textContent =
    `已选择较旧的 MCU 固件 ${release.label} · ${release.firmwareVersion}${suffix}。` +
    'Bridge 启动时会改写设备固件；仅在需要复现该固件版本的行为时保留此选择。';
}

// Returns true when the selection may be kept. Downgrades need an explicit
// confirmation because starting the Bridge reprograms the device.
async function confirmFirmwareSelection() {
  const release = findFirmwareRelease(elements.pxlogicFirmwareVersion.value);
  if (!release || release.latest || release.id === lastConfirmedFirmwareId) return true;
  const dialog = elements.firmwareDowngradeConfirmation;
  if (typeof dialog.showModal !== 'function') return true;
  const latest = latestFirmwareRelease();
  elements.firmwareDowngradeMessage.textContent =
    `将把 PXLogic 的 MCU 固件从 ${latest ? `${latest.label} · ${latest.firmwareVersion}` : '当前版本'}` +
    ` 改为 ${release.label} · ${release.firmwareVersion}（PXView 提交 ${release.pxviewCommit}，发布于 ${release.released}）。` +
    (release.notes ? ` ${release.notes}` : '');
  elements.firmwareDowngradeCheckbox.checked = false;
  elements.confirmFirmwareDowngradeButton.disabled = true;
  dialog.showModal();
  const accepted = await new Promise(resolve => {
    const finish = value => {
      elements.confirmFirmwareDowngradeButton.removeEventListener('click', onConfirm);
      elements.cancelFirmwareDowngradeButton.removeEventListener('click', onCancel);
      dialog.removeEventListener('cancel', onCancel);
      if (dialog.open) dialog.close();
      resolve(value);
    };
    const onConfirm = () => {
      if (!elements.firmwareDowngradeCheckbox.checked) return;
      finish(true);
    };
    const onCancel = event => {
      if (event) event.preventDefault();
      finish(false);
    };
    elements.confirmFirmwareDowngradeButton.addEventListener('click', onConfirm);
    elements.cancelFirmwareDowngradeButton.addEventListener('click', onCancel);
    dialog.addEventListener('cancel', onCancel);
  });
  if (!accepted) {
    const fallback = findFirmwareRelease(lastConfirmedFirmwareId) || latestFirmwareRelease();
    elements.pxlogicFirmwareVersion.value = fallback ? fallback.id : '';
    renderFirmwareWarning();
    return false;
  }
  lastConfirmedFirmwareId = release.id;
  return true;
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
elements.pxlogicFirmwareVersion.addEventListener('change', () => {
  void (async () => {
    const accepted = await confirmFirmwareSelection();
    renderFirmwareWarning();
    if (accepted) persistSettings();
  })();
});
elements.firmwareDowngradeCheckbox.addEventListener('change', () => {
  elements.confirmFirmwareDowngradeButton.disabled = !elements.firmwareDowngradeCheckbox.checked;
});
elements.thresholdVerified.addEventListener('change', () => {
  updateThresholdLabel();
  rememberThresholdProfile();
  persistSettings();});
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

elements.statusPanelButton.addEventListener('click', async () => {
  if (typeof api.toggleStatusPanel !== 'function') return;
  try {
    await api.toggleStatusPanel();
  } catch (error) {
    appendLog(`[client] 无法切换状态面板：${errorMessage(error)}`);
  }
});

elements.onboardingButton.addEventListener('click', () => openWizard(1));
// Escape closes a <dialog> natively, bypassing every button. Recording on close
// instead of per-button is the only way the walkthrough cannot silently reappear
// on the next launch after the user dismissed it.
elements.wizard.addEventListener('close', () => void recordOnboardingComplete());
elements.wizardBack.addEventListener('click', () => {
  wizardStep -= 1;
  renderWizardStep();
});
elements.wizardNext.addEventListener('click', () => {
  if (wizardStep >= elements.wizardSteps.length) {
    finishWizard();
    return;
  }
  wizardStep += 1;
  renderWizardStep();
  renderWizardReadouts();
});
// Skipping still counts as answered: re-opening the walkthrough uninvited is
// worse than trusting the user, and the header button brings it back.
elements.wizardSkip.addEventListener('click', () => finishWizard());

for (const item of [...elements.readinessItems, ...document.querySelectorAll('.wizard-jump')]) {
  item.addEventListener('click', () => {
    if (elements.wizard.open) elements.wizard.close();
    focusSetting(item.dataset.focus);
  });
}
elements.experimentalConfirmationCheckbox.addEventListener('change', () => {
  elements.continueExperimentalButton.disabled = !elements.experimentalConfirmationCheckbox.checked;
});
elements.logicRunningCheckbox.addEventListener('change', () => {
  elements.confirmLogicRunningButton.disabled = !elements.logicRunningCheckbox.checked;
});elements.cancelExperimentalButton.addEventListener('click', () => {
  clearExperimentalConfirmation();
  elements.experimentalConfirmation.close();
});
elements.experimentalConfirmation.addEventListener('cancel', clearExperimentalConfirmation);
elements.continueExperimentalButton.addEventListener('click', () => {
  if (!experimentalConfirmationToken || !elements.experimentalConfirmationCheckbox.checked) return;
  experimentalConfirmationToken.confirmed = true;
  elements.experimentalConfirmation.close();
  elements.startButton.click();
});
elements.startButton.addEventListener('click', async () => {
  try {
    if (isActive()) {
      if (needsRecovery()) {
        if (typeof api.restart !== 'function') throw new Error('当前客户端不支持安全重新初始化');
        if (!requestExperimentalConfirmation()) return;
        const pendingProfileFingerprint = consumeExperimentalConfirmationFingerprint();
        if (needsUsbReconnectRecovery()) {
          const previousDeviceId = elements.pxlogicDevice.value;
          rememberThresholdProfile(previousDeviceId);
          hardwareScanning = true;
          renderState(currentState);
          const hardware = await api.scanHardware(previousDeviceId);
          renderHardware(hardware);
          if (!isHardwareReady()) {
            throw new Error(hardware?.error || '重新扫描后 PXLogic 设备仍未就绪');
          }
          await api.saveSettings(readSettings());
          appendLog(
            `[client] USB 设备重新扫描完成: ${previousDeviceId || '未知地址'} -> ` +
            `${elements.pxlogicDevice.value}`,
          );
        }
        const state = await api.restart(readSettings(pendingProfileFingerprint));
        renderState(state);
      } else {
        await api.stop();
      }
    } else {
      if (!elements.pxlogicThreshold.reportValidity()) return;
      if (!requestExperimentalConfirmation()) return;
      if (!await resolveRunningLogic()) return;
      const pendingProfileFingerprint = consumeExperimentalConfirmationFingerprint();
      const state = await api.start(readSettings(pendingProfileFingerprint));
      renderState(state);
    }
  } catch (error) {
    const message = errorMessage(error);
    appendLog(`[client] ${message}`);
    elements.footerMessage.textContent = message;
  } finally {
    hardwareScanning = false;
    renderState(currentState);
  }
});

api.onState(renderState);
if (typeof api.onTelemetry === 'function') api.onTelemetry(renderTelemetry);
api.onLog(appendLog);

async function initialize() {
  if (typeof api.toggleStatusPanel === 'function') {
    elements.statusPanelButton.hidden = false;
  }
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
  renderFirmwareReleases(initial.firmwareReleases, initial.settings.pxlogicFirmwareId);
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
  maybeOpenWizard(initial.settings);
}

// Absent `guidance` means a host that does not persist walkthrough progress —
// the legacy Electron launcher — so the wizard stays out of its way entirely
// rather than reappearing on every launch.
function maybeOpenWizard(settings) {
  const guidance = settings?.guidance;
  if (!guidance || typeof api.completeOnboarding !== 'function') {
    elements.onboardingButton.hidden = true;
    return;
  }
  if (Number(guidance.onboardingCompletedVersion || 0) > 0) return;
  openWizard(1);
}

initialize().catch(error => {
  appendLog(`[client] ${errorMessage(error)}`);
  renderInspection(null);
});
