'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const {
  app,
  BrowserWindow,
  Menu,
  Tray,
  dialog,
  ipcMain,
  nativeImage,
  shell,
} = require('electron');

const DEFAULT_SETTINGS = Object.freeze({
  logicAppPath: '',
  portMode: 'auto',
  preferredPort: 12472,
  screenQuadrant: 3,
  maximizeLogicWindow: true,
  pxlogicDeviceId: '',
  pxlogicThresholdVolts: 1.8,
  pxlogicThresholdProfiles: {},
});
const MAX_PXLOGIC_THRESHOLD_VOLTS = 6.668;

let mainWindow;
let tray;
let bridgeProcess;
let bridgeState = {
  phase: 'stopped', actualPort: null, message: '待机', errorCode: null, recoveryAction: null,
};
let captureTelemetry = defaultCaptureTelemetry();
let logLines = [];
let quitting = false;

function bridgeRoot() {
  return app.isPackaged
    ? path.join(process.resourcesPath, 'tools', 'logic2-bridge')
    : path.resolve(__dirname, '..');
}

function bridgeApi() {
  return require(path.join(bridgeRoot(), 'index.cjs'));
}

function payloadRoot() {
  return app.isPackaged ? process.resourcesPath : path.resolve(__dirname, '..', '..', '..');
}

function scanHardware(preferredDeviceId = '') {
  const root = payloadRoot();
  const helper = path.join(
    root,
    'target',
    'release',
    process.platform === 'win32' ? 'usb_smoke.exe' : 'usb_smoke',
  );
  const firmware = path.join(root, 'resources', 'firmware', 'SCI_LOGIC.bin');
  const bitstream = path.join(root, 'resources', 'bitstreams', 'hspi_ddr.bin');
  const resetBitstream = path.join(root, 'resources', 'bitstreams', 'hspi_ddr_RST.bin');
  if (!fs.existsSync(helper)) {
    return {
      devices: [], selectedDeviceId: null,
      firmwareResourceReady: fs.existsSync(firmware),
      bitstreamResourcesReady: fs.existsSync(bitstream) && fs.existsSync(resetBitstream),
      error: `PXLogic helper 不存在: ${helper}`,
    };
  }
  const result = spawnSync(helper, ['--list-json'], {
    cwd: path.dirname(helper),
    env: {
      ...process.env,
      PXLOGIC_BITSTREAM_DIR: path.dirname(bitstream),
      PXLOGIC_MCU_FIRMWARE: firmware,
    },
    encoding: 'utf8',
    windowsHide: true,
  });
  if (result.status !== 0) {
    return {
      devices: [], selectedDeviceId: null,
      firmwareResourceReady: fs.existsSync(firmware),
      bitstreamResourcesReady: fs.existsSync(bitstream) && fs.existsSync(resetBitstream),
      error: `PXLogic 设备扫描失败: ${(result.stderr || result.error?.message || '').trim()}`,
    };
  }
  try {
    const devices = JSON.parse(result.stdout).map(device => ({
      id: device.id,
      vid: device.vid,
      pid: device.pid,
      bus: device.bus,
      address: device.address,
      label: device.label,
      ready: device.ready,
      manufacturer: device.manufacturer,
      product: device.product,
      serialNumber: device.serial_number,
      usbSpeed: device.usb_speed,
      logicMode: device.logic_mode,
      profileModel: device.profile_model,
      probeError: device.probe_error,
    }));
    const selected = devices.find(device => device.id === preferredDeviceId) ||
      devices.find(device => device.ready) || devices[0];
    return {
      devices,
      selectedDeviceId: selected?.id || null,
      firmwareResourceReady: fs.existsSync(firmware),
      bitstreamResourcesReady: fs.existsSync(bitstream) && fs.existsSync(resetBitstream),
      error: null,
    };
  } catch (error) {
    return {
      devices: [], selectedDeviceId: null,
      firmwareResourceReady: fs.existsSync(firmware),
      bitstreamResourcesReady: fs.existsSync(bitstream) && fs.existsSync(resetBitstream),
      error: `PXLogic 设备扫描响应无效: ${error.message}`,
    };
  }
}

function settingsPath() {
  return path.join(app.getPath('userData'), 'settings.json');
}

function normalizeSettings(value = {}) {
  const portMode = value.portMode === 'fixed' ? 'fixed' : 'auto';
  const preferredPort = Number(value.preferredPort);
  const screenQuadrant = Number(value.screenQuadrant);
  const explicitThreshold = Number(value.pxlogicThresholdVolts);
  const temporaryComparatorField = Number(value.pxlogicComparatorThresholdVolts);
  const pxlogicThresholdVolts = Number.isFinite(explicitThreshold)
    ? explicitThreshold
    : Number.isFinite(temporaryComparatorField)
      ? temporaryComparatorField
      : DEFAULT_SETTINGS.pxlogicThresholdVolts;
  const pxlogicThresholdProfiles = {};
  if (value.pxlogicThresholdProfiles && typeof value.pxlogicThresholdProfiles === 'object') {
    for (const [deviceId, candidate] of Object.entries(value.pxlogicThresholdProfiles)) {
      const volts = Number(candidate?.volts);
      if (!deviceId.trim() || !Number.isFinite(volts) ||
          volts < 0 || volts > MAX_PXLOGIC_THRESHOLD_VOLTS) continue;
      pxlogicThresholdProfiles[deviceId] = {
        volts,
        verified: candidate.verified === true,
        reference: typeof candidate.reference === 'string' ? candidate.reference.trim() : '',
      };
    }
  }
  return {
    logicAppPath: typeof value.logicAppPath === 'string' ? value.logicAppPath.trim() : '',
    portMode,
    preferredPort: Number.isInteger(preferredPort) && preferredPort >= 1 && preferredPort <= 65535
      ? preferredPort
      : DEFAULT_SETTINGS.preferredPort,
    screenQuadrant: [1, 2, 3, 4].includes(screenQuadrant)
      ? screenQuadrant
      : DEFAULT_SETTINGS.screenQuadrant,
    maximizeLogicWindow: value.maximizeLogicWindow !== false,
    pxlogicDeviceId: typeof value.pxlogicDeviceId === 'string'
      ? value.pxlogicDeviceId.trim()
      : '',
    pxlogicThresholdVolts: pxlogicThresholdVolts >= 0 &&
      pxlogicThresholdVolts <= MAX_PXLOGIC_THRESHOLD_VOLTS
      ? pxlogicThresholdVolts
      : DEFAULT_SETTINGS.pxlogicThresholdVolts,
    pxlogicThresholdProfiles,
  };
}

function loadSettings() {
  try {
    return normalizeSettings(JSON.parse(fs.readFileSync(settingsPath(), 'utf8')));
  } catch {
    return { ...DEFAULT_SETTINGS };
  }
}

function saveSettings(value) {
  const settings = normalizeSettings(value);
  fs.mkdirSync(path.dirname(settingsPath()), { recursive: true });
  const temporaryPath = `${settingsPath()}.tmp`;
  fs.writeFileSync(temporaryPath, `${JSON.stringify(settings, null, 2)}\n`);
  fs.renameSync(temporaryPath, settingsPath());
  return settings;
}

function inspectLogicApp(appPath) {
  if (!appPath) {
    return {
      path: '', version: null, supported: false, runnable: false, error: '未选择 Logic 2',
    };
  }
  const resolvedPath = path.resolve(appPath);
  let version = null;
  try {
    const api = bridgeApi();
    version = api.readAppVersion(resolvedPath);
    api.resolveRuntime({ appPath: resolvedPath });
    return { path: resolvedPath, version, supported: true, runnable: true, error: null };
  } catch (error) {
    return { path: resolvedPath, version, supported: false, runnable: false, error: error.message };
  }
}

function discoverLogicApps(savedPath = '') {
  const candidates = bridgeApi().installedAppCandidates();
  if (savedPath) candidates.unshift(savedPath);
  const seen = new Set();
  const applications = [];
  for (const candidate of candidates) {
    const normalized = path.resolve(candidate);
    if (seen.has(normalized)) continue;
    seen.add(normalized);
    if (!fs.existsSync(path.join(normalized, 'Contents', 'Info.plist'))) continue;
    applications.push(inspectLogicApp(normalized));
  }
  applications.sort((left, right) => Number(right.supported) - Number(left.supported));
  return applications;
}

function emit(channel, payload) {
  if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send(channel, payload);
}

function setBridgeState(next) {
  bridgeState = { ...bridgeState, ...next };
  emit('bridge:state', bridgeState);
  refreshTrayMenu();
}

function parseBridgeRuntimeEvent(line) {
  const prefix = '[logic2-bridge:event] ';
  if (!line.startsWith(prefix)) return null;
  try {
    return JSON.parse(line.slice(prefix.length));
  } catch {
    return null;
  }
}

function captureFailureMessage(code) {
  const messages = {
    PXLOGIC_RATE_MISMATCH: '实际采样率与 Logic 2 设置不一致',
    PXLOGIC_CHANNEL_MISMATCH: '实际通道映射与 Logic 2 设置不一致',
    PXLOGIC_CHANNEL_MAPPING_CHANGED: '实际通道映射与 Logic 2 设置不一致',
    PXLOGIC_CONVERSION_FAILED: 'PXLogic 数据转换失败',
    PXLOGIC_HELPER_START_FAILED: 'PXLogic 采集进程无法启动',
    PXLOGIC_HELPER_EXITED: 'PXLogic 采集进程异常退出',
    PXLOGIC_USB_REENUMERATED: '检测到 PXLogic 的 USB 地址发生变化，常见于电脑 USB 控制器、Hub 或设备重置。采集已安全停止，设备通常未损坏。请重新扫描并初始化 Bridge。',
  };
  return messages[code] || 'PXLogic 采集失败';
}

function defaultCaptureTelemetry() {
  return {
    status: 'idle',
    sampleRateHz: null,
    enabledChannels: [],
    channelSpan: null,
    thresholdVolts: null,
    triggerDescription: null,
    crossChunks: 0,
    convertedBytes: 0,
    windowCount: null,
    sampleCount: null,
    callbackCount: null,
    queuedBytes: null,
    injectedBytes: null,
    underflows: null,
    droppedBytes: null,
  };
}

function applyCaptureRuntimeEvent(event) {
  if (event.type === 'capture-starting') {
    captureTelemetry = {
      ...defaultCaptureTelemetry(),
      status: 'starting',
      sampleRateHz: event.sampleRateHz ?? null,
      enabledChannels: event.enabledChannels || [],
      thresholdVolts: event.thresholdVolts ?? null,
      triggerDescription: event.triggerDescription ?? null,
    };
  } else if (event.type === 'capture-started') {
    Object.assign(captureTelemetry, {
      status: 'streaming',
      sampleRateHz: event.sampleRateHz ?? captureTelemetry.sampleRateHz,
      enabledChannels: event.enabledChannels || captureTelemetry.enabledChannels,
      channelSpan: event.channelSpan ?? captureTelemetry.channelSpan,
      thresholdVolts: event.thresholdVolts ?? captureTelemetry.thresholdVolts,
      triggerDescription: event.triggerDescription ?? captureTelemetry.triggerDescription,
    });
  } else if (event.type === 'capture-progress') {
    for (const key of ['crossChunks', 'convertedBytes', 'windowCount', 'sampleCount']) {
      if (event[key] !== undefined) captureTelemetry[key] = event[key];
    }
  } else if (event.type === 'injection-progress') {
    for (const key of ['callbackCount', 'queuedBytes', 'injectedBytes', 'underflows', 'droppedBytes']) {
      if (event[key] !== undefined) captureTelemetry[key] = event[key];
    }
  } else if (event.type === 'capture-ended') {
    captureTelemetry.status = event.status || (event.failed ? 'error' : 'stopped');
    if (event.crossChunks !== undefined) captureTelemetry.crossChunks = event.crossChunks;
    if (event.convertedBytes !== undefined) captureTelemetry.convertedBytes = event.convertedBytes;
  } else if (event.type === 'capture-unavailable') {
    captureTelemetry.status = 'error';
  } else {
    return;
  }
  emit('bridge:telemetry', { ...captureTelemetry });
}

function appendLog(source, chunk) {
  const lines = String(chunk).split(/\r?\n/).filter(Boolean);
  for (const line of lines) {
    const entry = `[${source}] ${line}`;
    logLines.push(entry);
    if (logLines.length > 500) logLines.shift();
    emit('bridge:log', entry);
    const event = parseBridgeRuntimeEvent(line);
    if (event) applyCaptureRuntimeEvent(event);
    if (event?.type === 'capture-unavailable') {
      setBridgeState({
        phase: 'recovery',
        message: captureFailureMessage(event.code),
        errorCode: event.code,
        recoveryAction: event.recoveryAction || 'restart-bridge',
      });
    }
    const match = line.match(/Graph WebSocket ready: ws:\/\/127\.0\.0\.1:(\d+)\/saleae/);
    if (match) {
      setBridgeState({
        phase: 'running', actualPort: Number(match[1]), message: '已连接',
        errorCode: null, recoveryAction: null,
      });
    }
  }
}

function startBridge(rawSettings) {
  if (bridgeProcess) throw new Error('Bridge 已在运行');
  const settings = saveSettings(rawSettings);
  const logic = inspectLogicApp(settings.logicAppPath);
  if (!logic.supported) throw new Error(logic.error || 'Logic 2 安装无效');
  const hardware = scanHardware(settings.pxlogicDeviceId);
  if (hardware.error) throw new Error(hardware.error);
  const selectedDevice = hardware.devices.find(device => device.id === hardware.selectedDeviceId);
  if (!selectedDevice) throw new Error('未检测到 PXLogic 设备');
  if (!selectedDevice.ready) throw new Error(selectedDevice.probeError || 'PXLogic 设备尚未就绪');
  settings.pxlogicDeviceId = selectedDevice.id;
  saveSettings(settings);

  const args = [
    // Keep the entry relative to cwd.  Electron's Windows RunAsNode parser
    // can split an absolute `C:\\...` script path at the drive colon.
    'index.cjs',
    '--app', settings.logicAppPath,
    '--port', settings.portMode === 'auto' ? 'auto' : String(settings.preferredPort),
    '--pxlogic-device', settings.pxlogicDeviceId,
    '--hardware-threshold-volts', String(settings.pxlogicThresholdVolts),
  ];
  if (settings.maximizeLogicWindow) args.push('--maximize-window');
  else args.push('--screen-quadrant', String(settings.screenQuadrant));
  setBridgeState({
    phase: 'starting', actualPort: null, message: '正在启动',
    errorCode: null, recoveryAction: null,
  });
  captureTelemetry = defaultCaptureTelemetry();
  emit('bridge:telemetry', { ...captureTelemetry });
  logLines = [];
  bridgeProcess = spawn(process.execPath, args, {
    cwd: bridgeRoot(),
    env: { ...process.env, ELECTRON_RUN_AS_NODE: '1' },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  bridgeProcess.stdout.setEncoding('utf8');
  bridgeProcess.stderr.setEncoding('utf8');
  bridgeProcess.stdout.on('data', chunk => appendLog('bridge', chunk));
  // GraphServer and Logic write normal status output to stderr as well.
  bridgeProcess.stderr.on('data', chunk => appendLog('runtime', chunk));
  bridgeProcess.once('error', error => {
    appendLog('error', error.message);
    bridgeProcess = undefined;
    setBridgeState({
      phase: 'error', actualPort: null, message: error.message,
      errorCode: 'BRIDGE_START_FAILED', recoveryAction: 'export-diagnostics',
    });
  });
  bridgeProcess.once('exit', (code, signal) => {
    bridgeProcess = undefined;
    const failed = code !== 0 && !quitting;
    setBridgeState({
      phase: failed ? 'error' : 'stopped',
      actualPort: null,
      message: failed ? `Bridge 已退出（${signal || `代码 ${code}`}）` : '待机',
      errorCode: failed ? 'BRIDGE_PROCESS_EXITED' : null,
      recoveryAction: failed ? 'export-diagnostics' : null,
    });
  });
  return { ...bridgeState };
}

function stopBridge() {
  if (!bridgeProcess) return { ...bridgeState };
  setBridgeState({
    phase: 'stopping', message: '正在停止', errorCode: null, recoveryAction: null,
  });
  bridgeProcess.kill('SIGTERM');
  return { ...bridgeState };
}

async function restartBridge(settings) {
  if (bridgeProcess) {
    const processToStop = bridgeProcess;
    const stopped = new Promise((resolve, reject) => {
      const timeout = setTimeout(
        () => reject(new Error('等待 Bridge 停止超时，请先退出后再重新启动')),
        6000,
      );
      processToStop.once('exit', () => {
        clearTimeout(timeout);
        resolve();
      });
    });
    stopBridge();
    await stopped;
  }
  return startBridge(settings);
}

function readTextTail(filePath, maxBytes = 64 * 1024) {
  try {
    const contents = fs.readFileSync(filePath);
    return contents.subarray(Math.max(0, contents.length - maxBytes)).toString('utf8');
  } catch {
    return null;
  }
}

async function exportDiagnostics() {
  const generatedAtUnixSeconds = Math.floor(Date.now() / 1000);
  const result = await dialog.showSaveDialog(mainWindow, {
    title: '导出 PXLogic Bridge 诊断',
    defaultPath: `pxlogic-bridge-diagnostics-${generatedAtUnixSeconds}.json`,
    filters: [{ name: 'JSON', extensions: ['json'] }],
  });
  if (result.canceled || !result.filePath) return null;
  const settings = loadSettings();
  const logDirectory = path.join(
    app.getPath('home'), 'Library', 'Application Support', 'PXLogic', 'logic2-bridge',
  );
  const report = {
    schemaVersion: 1,
    generatedAtUnixSeconds,
    clientVersion: app.getVersion(),
    platform: process.platform,
    architecture: process.arch,
    settings,
    logic: inspectLogicApp(settings.logicAppPath),
    bridgeState,
    captureTelemetry,
    recentLogs: logLines,
    graphLogTail: readTextTail(path.join(logDirectory, 'graphio.log')),
  };
  fs.writeFileSync(result.filePath, `${JSON.stringify(report, null, 2)}\n`);
  return result.filePath;
}

function showWindow() {
  if (!mainWindow) return;
  if (app.dock) void app.dock.show();
  mainWindow.show();
  mainWindow.focus();
}

function refreshTrayMenu() {
  if (!tray) return;
  tray.setToolTip(`PXLogic Bridge - ${bridgeState.message}`);
  tray.setContextMenu(Menu.buildFromTemplate([
    { label: '显示 PXLogic Bridge', click: showWindow },
    { type: 'separator' },
    { label: bridgeState.message, enabled: false },
    { label: '停止 Bridge', enabled: Boolean(bridgeProcess), click: stopBridge },
    { type: 'separator' },
    {
      label: '退出',
      click: () => {
        quitting = true;
        stopBridge();
        app.quit();
      },
    },
  ]));
}

function createTray() {
  const iconPath = app.isPackaged
    ? path.join(process.resourcesPath, 'icon.png')
    : path.resolve(__dirname, '..', '..', '..', 'src-tauri', 'icons', 'icon.png');
  const image = nativeImage.createFromPath(iconPath).resize({ width: 18, height: 18 });
  image.setTemplateImage(true);
  tray = new Tray(image);
  tray.on('click', showWindow);
  refreshTrayMenu();
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 840,
    height: 780,
    minWidth: 720,
    minHeight: 660,
    title: 'PXLogic Bridge',
    backgroundColor: '#f2f3f5',
    show: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
    },
  });
  mainWindow.loadFile(path.join(__dirname, 'renderer', 'index.html'));
  mainWindow.once('ready-to-show', () => mainWindow.show());
  mainWindow.on('close', event => {
    if (!quitting) {
      event.preventDefault();
      mainWindow.hide();
      if (app.dock) app.dock.hide();
    }
  });
}

ipcMain.handle('client:initial-state', () => {
  const settings = loadSettings();
  const applications = discoverLogicApps(settings.logicAppPath);
  if (!settings.logicAppPath && applications.length) {
    const preferred = applications.find(item => item.supported) || applications[0];
    settings.logicAppPath = preferred.path;
    saveSettings(settings);
  }
  const hardware = scanHardware(settings.pxlogicDeviceId);
  if (hardware.selectedDeviceId && hardware.selectedDeviceId !== settings.pxlogicDeviceId) {
    settings.pxlogicDeviceId = hardware.selectedDeviceId;
    saveSettings(settings);
  }
  return { settings, applications, hardware, bridgeState, captureTelemetry, logs: logLines };
});
ipcMain.handle('client:save-settings', (_event, settings) => saveSettings(settings));
ipcMain.handle('logic:scan', (_event, savedPath) => discoverLogicApps(savedPath));
ipcMain.handle('logic:inspect', (_event, appPath) => inspectLogicApp(appPath));
ipcMain.handle('pxlogic:scan', (_event, preferredDeviceId) => scanHardware(preferredDeviceId));
ipcMain.handle('logic:browse', async () => {
  const result = await dialog.showOpenDialog(mainWindow, {
    title: '选择 Saleae Logic 2',
    buttonLabel: '选择',
    properties: ['openFile'],
    filters: [{ name: 'macOS 应用', extensions: ['app'] }],
  });
  if (result.canceled || !result.filePaths[0]) return null;
  return inspectLogicApp(result.filePaths[0]);
});
ipcMain.handle('bridge:start', (_event, settings) => startBridge(settings));
ipcMain.handle('bridge:restart', (_event, settings) => restartBridge(settings));
ipcMain.handle('bridge:stop', () => stopBridge());
ipcMain.handle('diagnostics:export', () => exportDiagnostics());
ipcMain.handle('logs:open', () => {
  const logDirectory = path.join(app.getPath('home'), 'Library', 'Application Support', 'PXLogic', 'logic2-bridge');
  fs.mkdirSync(logDirectory, { recursive: true });
  return shell.openPath(logDirectory);
});

app.whenReady().then(() => {
  createWindow();
  createTray();
});

app.on('activate', showWindow);
app.on('window-all-closed', () => {
  if (process.platform !== 'darwin' && !bridgeProcess) app.quit();
});
app.on('before-quit', () => {
  quitting = true;
  stopBridge();
});
