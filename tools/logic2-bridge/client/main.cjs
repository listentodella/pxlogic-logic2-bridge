'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
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
});

let mainWindow;
let tray;
let bridgeProcess;
let bridgeState = { phase: 'stopped', actualPort: null, message: '待机' };
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

function settingsPath() {
  return path.join(app.getPath('userData'), 'settings.json');
}

function normalizeSettings(value = {}) {
  const portMode = value.portMode === 'fixed' ? 'fixed' : 'auto';
  const preferredPort = Number(value.preferredPort);
  const screenQuadrant = Number(value.screenQuadrant);
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
  if (!appPath) return { path: '', version: null, supported: false, error: '未选择 Logic 2' };
  const resolvedPath = path.resolve(appPath);
  let version = null;
  try {
    const api = bridgeApi();
    version = api.readAppVersion(resolvedPath);
    api.resolveRuntime({ appPath: resolvedPath });
    return { path: resolvedPath, version, supported: true, error: null };
  } catch (error) {
    return { path: resolvedPath, version, supported: false, error: error.message };
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

function appendLog(source, chunk) {
  const lines = String(chunk).split(/\r?\n/).filter(Boolean);
  for (const line of lines) {
    const entry = `[${source}] ${line}`;
    logLines.push(entry);
    if (logLines.length > 500) logLines.shift();
    emit('bridge:log', entry);
    const match = line.match(/Graph WebSocket ready: ws:\/\/127\.0\.0\.1:(\d+)\/saleae/);
    if (match) {
      setBridgeState({ phase: 'running', actualPort: Number(match[1]), message: '已连接' });
    }
  }
}

function startBridge(rawSettings) {
  if (bridgeProcess) throw new Error('Bridge 已在运行');
  const settings = saveSettings(rawSettings);
  const logic = inspectLogicApp(settings.logicAppPath);
  if (!logic.supported) throw new Error(logic.error || 'Logic 2 安装无效');

  const args = [
    // Keep the entry relative to cwd.  Electron's Windows RunAsNode parser
    // can split an absolute `C:\\...` script path at the drive colon.
    'index.cjs',
    '--app', settings.logicAppPath,
    '--port', settings.portMode === 'auto' ? 'auto' : String(settings.preferredPort),
  ];
  if (settings.maximizeLogicWindow) args.push('--maximize-window');
  else args.push('--screen-quadrant', String(settings.screenQuadrant));
  setBridgeState({ phase: 'starting', actualPort: null, message: '正在启动' });
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
    setBridgeState({ phase: 'error', actualPort: null, message: error.message });
  });
  bridgeProcess.once('exit', (code, signal) => {
    bridgeProcess = undefined;
    const failed = code !== 0 && !quitting;
    setBridgeState({
      phase: failed ? 'error' : 'stopped',
      actualPort: null,
      message: failed ? `Bridge 已退出（${signal || `代码 ${code}`}）` : '待机',
    });
  });
  return { ...bridgeState };
}

function stopBridge() {
  if (!bridgeProcess) return { ...bridgeState };
  setBridgeState({ phase: 'stopping', message: '正在停止' });
  bridgeProcess.kill('SIGTERM');
  return { ...bridgeState };
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
    height: 680,
    minWidth: 720,
    minHeight: 590,
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
  return { settings, applications, bridgeState, logs: logLines };
});
ipcMain.handle('client:save-settings', (_event, settings) => saveSettings(settings));
ipcMain.handle('logic:scan', (_event, savedPath) => discoverLogicApps(savedPath));
ipcMain.handle('logic:inspect', (_event, appPath) => inspectLogicApp(appPath));
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
ipcMain.handle('bridge:stop', () => stopBridge());
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
