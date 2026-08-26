#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const {
  PxlogicCaptureController,
  bridgeEventLine,
  createLineReader,
  preparePxlogicDevice,
} = require('./lib/capture-controller.cjs');
const {
  findGraphServerBinary,
  loadCompatibilityManifest,
  matchCompatibilityProfile,
  readLogicVersionFromInstallation,
} = require('./lib/compatibility.cjs');
const { normalizeEnabledChannels } = require('./lib/logic-format.cjs');
const { GraphActionGuard } = require('./lib/graph-action-guard.cjs');
const { GraphLogMonitor } = require('./lib/diagnostics.cjs');
const { startWebSocketProxy } = require('./lib/websocket-proxy.cjs');

const bridgeRoot = __dirname;
const pxlogicRoot = path.resolve(bridgeRoot, '..', '..');

function parseInjectionStats(line) {
  const match = String(line).match(
    /^\[logic2-bridge:inject\] callback=(\d+) buffer=(\d+) injected=(\d+) queued=(\d+) total=(\d+) underflows=(\d+) dropped=(\d+)$/,
  );
  if (!match) return null;
  return {
    type: 'injection-progress',
    callbackCount: Number(match[1]),
    callbackBufferBytes: Number(match[2]),
    callbackInjectedBytes: Number(match[3]),
    queuedBytes: Number(match[4]),
    injectedBytes: Number(match[5]),
    underflows: Number(match[6]),
    droppedBytes: Number(match[7]),
  };
}

function parseArguments(argv) {
  const result = {
    appPath: process.env.SALEAE_LOGIC_APP,
    port: 0,
    backendPort: undefined,
    pxlogicHelper: process.env.PXLOGIC_USB_SMOKE ||
      path.join(
        pxlogicRoot,
        'target',
        'release',
        process.platform === 'win32' ? 'usb_smoke.exe' : 'usb_smoke',
      ),
    pxlogicDevice: undefined,
    pxlogicSerialNumber: undefined,
    pxlogicUsbSpeed: undefined,
    pxlogicLogicMode: undefined,
    bitstreams: process.env.PXLOGIC_BITSTREAM_DIR ||
      path.join(pxlogicRoot, 'resources', 'bitstreams'),
    firmware: process.env.PXLOGIC_MCU_FIRMWARE ||
      path.join(pxlogicRoot, 'resources', 'firmware', 'SCI_LOGIC.bin'),
    enabledChannels: [0, 1, 2, 3],
    sampleRateHz: 25_000_000,
    thresholdVolts: 2.0,
    hardwareThresholdVolts: undefined,
    captureWindowMs: 1000,
    scanSaleaeDevices: false,
    screenQuadrant: undefined,
    maximizeWindow: true,
    remoteDebuggingPort: undefined,
    allowPendingProfile: false,
    compatibilityProfiles: process.env.PXLOGIC_COMPATIBILITY_PROFILES,
    dryRun: false,
  };

  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--app') result.appPath = path.resolve(argv[++index]);
    else if (argument === '--port') {
      const value = argv[++index];
      result.port = value === 'auto' ? 0 : Number(value);
    }
    else if (argument === '--backend-port') result.backendPort = Number(argv[++index]);
    else if (argument === '--pxlogic-helper') result.pxlogicHelper = path.resolve(argv[++index]);
    else if (argument === '--pxlogic-device') result.pxlogicDevice = argv[++index];
    else if (argument === '--pxlogic-serial') result.pxlogicSerialNumber = argv[++index];
    else if (argument === '--pxlogic-usb-speed') result.pxlogicUsbSpeed = argv[++index];
    else if (argument === '--pxlogic-logic-mode') result.pxlogicLogicMode = Number(argv[++index]);
    else if (argument === '--bitstreams') result.bitstreams = path.resolve(argv[++index]);
    else if (argument === '--firmware') result.firmware = path.resolve(argv[++index]);
    else if (argument === '--enabled-channels') {
      result.enabledChannels = normalizeEnabledChannels(argv[++index]);
    } else if (argument === '--sample-rate') result.sampleRateHz = Number(argv[++index]);
    else if (argument === '--threshold-volts') result.thresholdVolts = Number(argv[++index]);
    else if (argument === '--hardware-threshold-volts') {
      result.hardwareThresholdVolts = Number(argv[++index]);
    }
    else if (argument === '--capture-window-ms') result.captureWindowMs = Number(argv[++index]);
    else if (argument === '--scan-saleae-devices') result.scanSaleaeDevices = true;
    else if (argument === '--screen-quadrant') {
      result.screenQuadrant = Number(argv[++index]);
      result.maximizeWindow = false;
    }
    else if (argument === '--maximize-window') result.maximizeWindow = true;
    else if (argument === '--remote-debugging-port') {
      result.remoteDebuggingPort = Number(argv[++index]);
    } else if (argument === '--allow-pending-profile') result.allowPendingProfile = true;
    else if (argument === '--compatibility-profiles') {
      result.compatibilityProfiles = path.resolve(argv[++index]);
    }
    else if (argument === '--dry-run') result.dryRun = true;
    else if (argument === '--help' || argument === '-h') result.help = true;
    else throw new Error(`Unknown argument: ${argument}`);
  }

  for (const [name, port] of [['port', result.port], ['backend port', result.backendPort]]) {
    const minimum = name === 'port' ? 0 : 1;
    if (port !== undefined && (!Number.isInteger(port) || port < minimum || port > 65535)) {
      throw new Error(`Invalid ${name}: ${port}`);
    }
  }
  if (!Number.isInteger(result.sampleRateHz) || result.sampleRateHz <= 0) {
    throw new Error(`Invalid sample rate: ${result.sampleRateHz}`);
  }
  if (!Number.isFinite(result.thresholdVolts) ||
      result.thresholdVolts < 0 || result.thresholdVolts > 6.668) {
    throw new Error(`Invalid threshold voltage: ${result.thresholdVolts}`);
  }
  if (result.hardwareThresholdVolts !== undefined &&
      (!Number.isFinite(result.hardwareThresholdVolts) ||
       result.hardwareThresholdVolts < 0 || result.hardwareThresholdVolts > 6.668)) {
    throw new Error(
      `Invalid PXLogic hardware threshold: ${result.hardwareThresholdVolts}`,
    );
  }
  if (!Number.isInteger(result.captureWindowMs) || result.captureWindowMs < 10) {
    throw new Error(`Invalid capture window: ${result.captureWindowMs}`);
  }
  if (result.screenQuadrant !== undefined && ![1, 2, 3, 4].includes(result.screenQuadrant)) {
    throw new Error(`Invalid screen quadrant: ${result.screenQuadrant}`);
  }
  if (result.remoteDebuggingPort !== undefined &&
      (!Number.isInteger(result.remoteDebuggingPort) ||
       result.remoteDebuggingPort < 1 || result.remoteDebuggingPort > 65535)) {
    throw new Error(`Invalid remote debugging port: ${result.remoteDebuggingPort}`);
  }
  return result;
}

function printHelp() {
  console.log(`PXLogic bridge for the official Saleae Logic application

Usage:
  node tools/logic2-bridge/index.cjs --app "/path/to/Logic.app" [options]

Options:
  --app PATH                 Official Saleae Logic app; auto-detected when installed
  --port PORT|auto           Logic-facing Graph WebSocket port (default: auto)
  --backend-port PORT        Private GraphServer port (default: automatic)
  --pxlogic-helper FILE      PXLogic usb_smoke executable
  --pxlogic-device ID        Select one PXLogic USB device
  --pxlogic-serial SERIAL    Stable PXLogic USB serial used across re-enumeration
  --pxlogic-usb-speed VALUE  PXLogic USB speed from the device probe
  --pxlogic-logic-mode N     PXLogic logic mode from the device probe
  --bitstreams DIR           PXLogic FPGA bitstream directory
  --firmware FILE            PXLogic MCU firmware image
  --enabled-channels LIST    Initial channels before Logic sends settings (default: 0,1,2,3)
  --sample-rate HZ           Initial sample rate (default: 25000000)
  --threshold-volts V        Initial nominal I/O level (default: 2.0)
  --hardware-threshold-volts V
                             Fixed PXLogic voltage threshold: 0..6.668 V
  --capture-window-ms MS     PXLogic stream re-arm window (default: 1000)
  --scan-saleae-devices      Also let GraphServer scan physical Saleae devices
  --maximize-window          Maximize Logic after launch (default)
  --screen-quadrant N        Place Logic in screen quadrant 1-4 instead
  --remote-debugging-port N  Enable Chromium remote debugging for this Logic process
  --allow-pending-profile    Run an exact-match experimental profile for live validation
  --compatibility-profiles FILE
                             Add offline local candidate profiles from FILE
  --dry-run                  Validate paths, version, and native host build without launching
  --help                     Show this help

No npm install is required.`);
}

function windowsMaximizeScript(pid) {
  return `Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class PxlogicWindow {
  [DllImport("user32.dll")]
  public static extern bool ShowWindowAsync(IntPtr window, int command);
}
'@
$deadline = [DateTime]::UtcNow.AddSeconds(10)
do {
  $process = Get-Process -Id ${pid} -ErrorAction SilentlyContinue
  if ($process -and $process.MainWindowHandle -ne 0) {
    [void][PxlogicWindow]::ShowWindowAsync([IntPtr]$process.MainWindowHandle, 3)
    exit 0
  }
  Start-Sleep -Milliseconds 200
} while ([DateTime]::UtcNow -lt $deadline)`;
}

function maximizeWindowsLogicWindow(pid) {
  if (process.platform !== 'win32' || !Number.isInteger(pid) || pid <= 0) return;
  // Electron creates its main window after process start, so wait briefly for its HWND.
  const maximizer = spawn('powershell.exe', [
    '-NoLogo',
    '-NoProfile',
    '-NonInteractive',
    '-WindowStyle',
    'Hidden',
    '-Command',
    windowsMaximizeScript(pid),
  ], {
    stdio: 'ignore',
    windowsHide: true,
  });
  maximizer.unref();
}

function macMaximizeScript(pid) {
  return `ObjC.import('AppKit');
function run() {
  const pid = ${pid};
  const screen = $.NSScreen.mainScreen;
  const frame = screen.frame;
  const visible = screen.visibleFrame;
  const bounds = {
    x: Number(visible.origin.x),
    y: Number(frame.size.height - visible.origin.y - visible.size.height),
    width: Number(visible.size.width),
    height: Number(visible.size.height),
  };
  const systemEvents = Application('System Events');
  for (let attempt = 0; attempt < 50; attempt += 1) {
    const matches = systemEvents.applicationProcesses.whose({ unixId: pid })();
    if (matches.length > 0) {
      const process = matches[0];
      process.frontmost = true;
      const windows = process.windows();
      if (windows.length > 0) {
        windows[0].position = [bounds.x, bounds.y];
        windows[0].size = [bounds.width, bounds.height];
        return;
      }
    }
    delay(0.2);
  }
}`;
}

function maximizeMacLogicWindow(pid) {
  if (process.platform !== 'darwin' || !Number.isInteger(pid) || pid <= 0) return;
  const maximizer = spawn('/usr/bin/osascript', [
    '-l',
    'JavaScript',
    '-e',
    macMaximizeScript(pid),
  ], { stdio: 'ignore' });
  maximizer.unref();
}

function maximizeLogicWindow(pid) {
  maximizeWindowsLogicWindow(pid);
  maximizeMacLogicWindow(pid);
}

function requirePath(target, label, kind = 'any') {
  let stat;
  try {
    stat = fs.statSync(target);
  } catch {
    throw new Error(`${label} was not found: ${target}`);
  }
  if (kind === 'file' && !stat.isFile()) throw new Error(`${label} is not a file: ${target}`);
  if (kind === 'directory' && !stat.isDirectory()) {
    throw new Error(`${label} is not a directory: ${target}`);
  }
}

function installedAppCandidates() {
  const names = ['Saleae Logic.app', 'Logic 2.app', 'Logic.app'];
  const roots = ['/Applications', path.join(os.homedir(), 'Applications')];
  const candidates = roots.flatMap(root => names.map(name => path.join(root, name)));
  const spotlight = spawnSync(
    'mdfind',
    ['kMDItemCFBundleIdentifier == "com.saleae.saleae"'],
    { encoding: 'utf8' },
  );
  if (spotlight.status === 0) {
    candidates.push(...spotlight.stdout.split(/\r?\n/).filter(Boolean));
  }
  return [...new Set(candidates)];
}

function readMacBundleIdentifier(appPath) {
  if (process.platform !== 'darwin') return null;
  const plist = path.join(appPath, 'Contents', 'Info.plist');
  const result = spawnSync(
    '/usr/libexec/PlistBuddy',
    ['-c', 'Print :CFBundleIdentifier', plist],
    { encoding: 'utf8' },
  );
  if (result.status !== 0) return null;
  const identifier = result.stdout.trim();
  return identifier || null;
}

function isLogicAppBundle(appPath) {
  try {
    if (!fs.statSync(appPath).isDirectory()) return false;
  } catch {
    return false;
  }
  if (process.platform === 'darwin') {
    return readMacBundleIdentifier(appPath) === 'com.saleae.saleae';
  }
  return fs.existsSync(path.join(appPath, 'Contents', 'Info.plist'));
}

function logicAppCandidatesFromPath(selectedPath) {
  const normalized = path.resolve(selectedPath);
  if (isLogicAppBundle(normalized)) return [normalized];
  if (process.platform !== 'darwin') return [];
  let entries;
  try {
    entries = fs.readdirSync(normalized, { withFileTypes: true });
  } catch {
    return [];
  }
  return entries
    .filter(entry => entry.isDirectory() && entry.name.toLowerCase().endsWith('.app'))
    .map(entry => path.join(normalized, entry.name))
    .filter(isLogicAppBundle)
    .sort();
}

function resolveAppPath(explicitPath) {
  if (explicitPath) {
    const selected = logicAppCandidatesFromPath(explicitPath);
    return selected[0] || path.resolve(explicitPath);
  }
  const found = installedAppCandidates().find(candidate =>
    fs.existsSync(path.join(candidate, 'Contents', 'Info.plist')),
  );
  if (!found) {
    throw new Error('Saleae Logic was not found; pass --app "/path/to/Logic.app"');
  }
  return found;
}

function readAppVersion(appPath) {
  if (process.platform !== 'darwin') {
    const version = readLogicVersionFromInstallation(appPath);
    if (!version) throw new Error(`Could not read Logic version from ${appPath}`);
    return version;
  }
  const plist = path.join(appPath, 'Contents', 'Info.plist');
  const result = spawnSync(
    '/usr/libexec/PlistBuddy',
    ['-c', 'Print :CFBundleShortVersionString', plist],
    { encoding: 'utf8' },
  );
  if (result.status !== 0) {
    throw new Error(`Could not read Logic version from ${plist}: ${result.stderr.trim()}`);
  }
  return result.stdout.trim();
}

function logicExecutable(appPath) {
  if (process.platform === 'darwin') return path.join(appPath, 'Contents', 'MacOS', 'Logic');
  if (fs.existsSync(appPath) && fs.statSync(appPath).isFile()) return appPath;
  const candidates = process.platform === 'win32'
    ? [path.join(appPath, 'Logic.exe'), path.join(appPath, 'Logic')]
    : [
      path.join(appPath, 'usr', 'lib', 'logic', 'Logic'),
      path.join(appPath, 'usr', 'lib', 'logic', 'Logic.bin'),
      path.join(appPath, 'Logic'),
    ];
  return candidates.find(candidate => fs.existsSync(candidate)) || candidates[0];
}

function resolveRuntimeVersion(detectedVersion, profile) {
  if (typeof detectedVersion === 'string' && detectedVersion.trim()) {
    return detectedVersion.trim();
  }
  if (typeof profile?.logicVersion === 'string' && profile.logicVersion.trim()) {
    return profile.logicVersion.trim();
  }
  return 'unknown';
}

function loadRuntimeCompatibilityProfiles(localProfilesPath) {
  const builtIn = loadCompatibilityManifest();
  if (!Number.isSafeInteger(builtIn.analyzerVersion) || builtIn.analyzerVersion < 1) {
    throw new Error('Built-in compatibility manifest has an invalid analyzer version');
  }
  if (!localProfilesPath) return builtIn.profiles;
  const local = loadCompatibilityManifest(localProfilesPath);
  if (local.analyzerVersion !== builtIn.analyzerVersion) {
    throw new Error(
      `Local compatibility profile analyzer version ${local.analyzerVersion ?? 'missing'} ` +
      `does not match ${builtIn.analyzerVersion}`,
    );
  }
  const localProfiles = local.profiles.filter(profile =>
    ['candidate', 'locally-verified'].includes(profile.hook?.status));
  return [...builtIn.profiles, ...localProfiles];
}

function resolveRuntime(options) {
  const appPath = resolveAppPath(options.appPath);
  let detectedVersion;
  let versionError;
  try {
    detectedVersion = readAppVersion(appPath);
  } catch (error) {
    versionError = error;
  }
  const executable = logicExecutable(appPath);
  const sharedLibrary = findGraphServerBinary(appPath);
  if (!sharedLibrary) {
    throw new Error(`GraphServer binary was not found below ${appPath}`);
  }
  const runtimeRoot = path.dirname(sharedLibrary);
  const pythonHome = path.join(runtimeRoot, 'pythonlibs');
  requirePath(executable, 'Logic executable', 'file');
  requirePath(sharedLibrary, 'GraphServer library', 'file');
  requirePath(pythonHome, 'Logic Python runtime', 'directory');
  const compatibility = matchCompatibilityProfile({
    logicVersion: detectedVersion,
    platform: process.platform,
    architecture: process.arch,
    graphPath: sharedLibrary,
    profiles: loadRuntimeCompatibilityProfiles(options.compatibilityProfiles),
  });
  if (!compatibility.profile) {
    const identity = compatibility.fingerprint.identity || 'unknown';
    const version = resolveRuntimeVersion(detectedVersion, null);
    throw new Error(
      `Unsupported Logic ${version} GraphServer identity ` +
      `${compatibility.fingerprint.identityKind || 'unknown'} ` +
      `${identity} (sha256 ${compatibility.fingerprint.sha256}); refusing to patch an unverified build.`,
    );
  }
  const version = resolveRuntimeVersion(detectedVersion, compatibility.profile);
  if (!detectedVersion) {
    console.warn(
      `[logic2-bridge] ${versionError?.message || 'Logic version metadata was unavailable'}; ` +
      `using exact GraphServer profile metadata (${version})`,
    );
  }
  const experimentalStatuses = new Set([
    'pending-live-validation',
    'candidate',
    'locally-verified',
  ]);
  const pendingValidation = experimentalStatuses.has(compatibility.profile.hook.status);
  if (!compatibility.supported && !(options.allowPendingProfile && pendingValidation)) {
    throw new Error(
      `GraphServer profile ${compatibility.profile.id} is not live-validated: ` +
      `${compatibility.profile.hook.validation}. ` +
      'Use --allow-pending-profile only for a controlled hardware validation run.',
    );
  }
  if (pendingValidation) {
    console.warn(
      `[logic2-bridge] EXPERIMENTAL GraphServer profile ${compatibility.profile.id}: ` +
      compatibility.profile.hook.validation,
    );
  }
  const hook = compatibility.profile.hook;
  if (!/^0x[0-9a-f]+$/i.test(hook.onDataBufferOffset || '')) {
    throw new Error(`GraphServer profile ${compatibility.profile.id} has an invalid hook offset`);
  }
  if (!/^(?:[0-9a-f]{2})+$/i.test(hook.prologueHex || '')) {
    throw new Error(`GraphServer profile ${compatibility.profile.id} has an invalid hook prologue`);
  }
  return { appPath, version, runtimeRoot, executable, sharedLibrary, pythonHome, compatibility };
}

function nativeHookArguments(runtime) {
  const profile = runtime.compatibility.profile;
  return [
    profile.id,
    profile.graph.identity,
    profile.hook.onDataBufferOffset,
    profile.hook.prologueHex,
  ];
}

function ensureNativeHost() {
  const supportedNativeHost =
    (process.platform === 'darwin' && process.arch === 'arm64') ||
    (process.platform === 'linux' && process.arch === 'x64') ||
    (process.platform === 'win32' && process.arch === 'x64');
  if (!supportedNativeHost) {
    throw new Error('PXLogic GraphServer injection host is not implemented for this platform');
  }
  const source = path.join(bridgeRoot, 'native', 'graph-host.c');
  const outputDirectory = path.join(bridgeRoot, 'build');
  const executable = path.join(
    outputDirectory,
    process.platform === 'win32' ? 'graph-host.exe' : 'graph-host',
  );
  fs.mkdirSync(outputDirectory, { recursive: true });
  const sourceExists = fs.existsSync(source);
  if (process.platform === 'win32') {
    if (!fs.existsSync(executable)) {
      throw new Error(`Prebuilt native GraphServer host was not found: ${executable}`);
    }
    return executable;
  }
  const needsBuild = !fs.existsSync(executable) ||
    (sourceExists && fs.statSync(source).mtimeMs > fs.statSync(executable).mtimeMs);
  if (needsBuild) {
    if (!sourceExists) {
      throw new Error(`Prebuilt native GraphServer host was not found: ${executable}`);
    }
    console.log('[logic2-bridge] compiling version-locked native GraphServer host');
    const compiler = process.platform === 'darwin' ? 'xcrun' : 'cc';
    const compilerArgs = process.platform === 'darwin'
      ? ['clang']
      : [];
    compilerArgs.push('-std=c11', '-O2', '-Wall', '-Wextra', source, '-o', executable);
    if (process.platform === 'linux') compilerArgs.push('-ldl', '-pthread');
    const result = spawnSync(
      compiler,
      compilerArgs,
      { cwd: bridgeRoot, encoding: 'utf8' },
    );
    if (result.status !== 0) {
      throw new Error(`Failed to compile native host:\n${result.stderr || result.stdout}`);
    }
    if (process.platform === 'darwin') {
      // clang emits a "linker-signed" ad-hoc signature, which stops validating
      // once the binary is copied by a process carrying provenance. macOS then
      // kills the copy with SIGKILL (Code Signature Invalid) before it prints a
      // line. Re-signing yields a genuine ad-hoc signature that survives copying.
      // Only the binary this call just produced is touched, so a signed
      // application bundle is never modified.
      const signed = spawnSync(
        'codesign',
        ['--force', '--sign', '-', executable],
        { cwd: bridgeRoot, encoding: 'utf8' },
      );
      if (signed.status !== 0) {
        throw new Error(
          `Failed to sign native host:\n${signed.stderr || signed.stdout}`,
        );
      }
    }
  }
  return executable;
}

function findAvailableTcpPort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      const port = typeof address === 'object' && address ? address.port : undefined;
      server.close(error => {
        if (error) reject(error);
        else if (!port) reject(new Error('Could not allocate a GraphServer port'));
        else resolve(port);
      });
    });
  });
}

function logicProcessEnvironment(environment = process.env) {
  const result = { ...environment };
  delete result.ELECTRON_RUN_AS_NODE;
  return result;
}

function nativeGraphStartupTimeoutMs(environment = process.env) {
  const configured = Number(environment.PXLOGIC_GRAPH_START_TIMEOUT_MS);
  if (Number.isInteger(configured) && configured >= 1000 && configured <= 300000) {
    return configured;
  }
  return process.platform === 'win32' ? 60000 : 15000;
}

function waitForNativeGraph(host, expectedPort, timeoutMs = 15000) {
  return new Promise((resolve, reject) => {
    let settled = false;
    let probeTimer;
    let timeout;
    const finish = (error, port) => {
      if (settled) return;
      settled = true;
      clearTimeout(timeout);
      clearTimeout(probeTimer);
      if (error) reject(error);
      else resolve(port);
    };
    const probe = () => {
      if (settled) return;
      const socket = net.createConnection({ host: '127.0.0.1', port: expectedPort });
      socket.unref();
      socket.once('connect', () => {
        socket.destroy();
        console.log(
          `[logic2-bridge] native GraphServer is listening on port ${expectedPort} ` +
          'before CreateGraphServer returned',
        );
        finish(null, expectedPort);
      });
      socket.once('error', () => {
        socket.destroy();
        if (!settled) probeTimer = setTimeout(probe, 250);
      });
    };
    timeout = setTimeout(() => {
      finish(new Error(expectedPort
        ? `Timed out waiting for the native GraphServer on port ${expectedPort}`
        : 'Timed out waiting for the native GraphServer to report its assigned port'));
    }, timeoutMs);
    createLineReader(host.stdout, line => {
      const match = line.match(/^GRAPH_WS_READY ws:\/\/127\.0\.0\.1:(\d+)\/saleae$/);
      if (match) {
        finish(null, Number(match[1]));
      } else {
        console.log(line);
      }
    });
    host.once('exit', (code, signal) => {
      finish(new Error(
        `Native GraphServer exited before ready (${signal || `code ${code}`})`,
      ));
    });
    host.once('error', error => {
      finish(error);
    });
    if (expectedPort) probe();
  });
}

async function waitForExit(child) {
  if (child.exitCode !== null) return { code: child.exitCode, signal: child.signalCode };
  return new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('exit', (code, signal) => resolve({ code, signal }));
  });
}

async function main() {
  const options = parseArguments(process.argv.slice(2));
  if (options.help) {
    printHelp();
    return;
  }

  const runtime = resolveRuntime(options);
  requirePath(options.pxlogicHelper, 'PXLogic USB helper', 'file');
  requirePath(options.bitstreams, 'PXLogic bitstream directory', 'directory');
  requirePath(options.firmware, 'PXLogic MCU firmware', 'file');
  const nativeHost = ensureNativeHost();
  console.log(`[logic2-bridge] official Logic ${runtime.version}: ${runtime.appPath}`);
  console.log(`[logic2-bridge] GraphServer profile: ${runtime.compatibility.profile.id}`);
  console.log(`[logic2-bridge] PXLogic helper: ${options.pxlogicHelper}`);
  if (options.dryRun) {
    console.log('[logic2-bridge] validation successful');
    return;
  }

  // PXView prepares the FPGA when a device is opened, not on every capture.
  // Keep the prepared state in the hardware for this Bridge process and make
  // every later Start/Stop operation capture-only.
  await preparePxlogicDevice(options);

  const stateRoot = process.platform === 'darwin'
    ? path.join(os.homedir(), 'Library', 'Application Support', 'PXLogic', 'logic2-bridge')
    : process.platform === 'win32'
      ? path.join(process.env.LOCALAPPDATA || os.homedir(), 'PXLogic', 'logic2-bridge')
      : path.join(process.env.XDG_STATE_HOME || path.join(os.homedir(), '.local', 'state'), 'pxlogic', 'logic2-bridge');
  const calibrationRoot = path.join(stateRoot, 'mso-calibration');
  const logPath = path.join(stateRoot, 'graphio.log');
  fs.mkdirSync(calibrationRoot, { recursive: true });
  const requestedBackendPort = options.backendPort ?? await findAvailableTcpPort();
  // Logic 2.4.46's Windows GraphServer completes initialization only when it
  // selects its own port. The host reports that selected port before the
  // public proxy is started, so no fixed backend port is needed on Windows.
  const backendPort = process.platform === 'win32' ? 0 : requestedBackendPort;
  if (backendPort !== 0 && backendPort === options.port) {
    throw new Error('The public proxy and private GraphServer ports must differ');
  }

  const graphLogMonitor = new GraphLogMonitor(logPath, {
    onFailure: event => console.error(bridgeEventLine(event)),
  });
  // Establish the session boundary before starting GraphServer. The file is
  // intentionally append-only across Bridge runs, so historical assertions
  // must never be replayed as a current failure.
  graphLogMonitor.poll();
  const host = spawn(nativeHost, [
    runtime.sharedLibrary,
    runtime.pythonHome,
    logPath,
    calibrationRoot,
    String(backendPort),
    options.scanSaleaeDevices ? '1' : '0',
    ...nativeHookArguments(runtime),
  ], {
    // GraphServer resolves several Windows resources relative to the official
    // Logic installation. Running the host from the bridge state directory
    // lets the DLL load, but can leave CreateGraphServer blocked while it
    // searches for its bundled runtime files.
    cwd: process.platform === 'win32' ? runtime.appPath : stateRoot,
    env: process.env,
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: process.platform === 'win32',
  });
  graphLogMonitor.start();
  createLineReader(host.stderr, line => {
    console.error(line);
    const stats = parseInjectionStats(line);
    if (stats) console.error(bridgeEventLine(stats));
  });
  if (process.platform === 'win32') {
    console.log('[logic2-bridge] Windows GraphServer will select its backend port');
  }
  host.stdin.on('error', error => {
    if (error.code !== 'EPIPE') console.error(`[logic2-bridge] host input failed: ${error.message}`);
  });

  let controller;
  let proxy;
  let logic;
  let stopping = false;
  let stopPromise;
  const stop = signal => {
    if (stopPromise) return stopPromise;
    stopping = true;
    stopPromise = (async () => {
      await controller?.shutdown();
      await proxy?.close();
      if (logic?.exitCode === null) logic.kill(signal);
      if (host.exitCode === null) host.kill(signal);
    })();
    return stopPromise;
  };
  process.once('SIGINT', () => void stop('SIGINT'));
  process.once('SIGTERM', () => void stop('SIGTERM'));

  try {
    const startupTimeoutMs = nativeGraphStartupTimeoutMs();
    console.log(
      `[logic2-bridge] waiting up to ${startupTimeoutMs} ms for native GraphServer ` +
      `on port ${backendPort}`,
    );
    const actualBackendPort = await waitForNativeGraph(
      host,
      backendPort,
      startupTimeoutMs,
    );
    controller = new PxlogicCaptureController(options, host);
    const graphActionGuard = new GraphActionGuard();
    proxy = await startWebSocketProxy({
      port: options.port,
      backendPort: actualBackendPort,
      observeText: async message => {
        const transformed = graphActionGuard.transform(message);
        await controller.observeRequest(message);
        return transformed;
      },
    });
    if (options.port !== 0 && proxy.port !== options.port) {
      console.log(
        `[logic2-bridge] requested port ${options.port} was unavailable; using ${proxy.port}`,
      );
    }
    console.log(`[logic2-bridge] Graph WebSocket ready: ws://127.0.0.1:${proxy.port}/saleae`);

    const logicArguments = ['--useExistingGraph', '--graphPort', String(proxy.port)];
    if (options.maximizeWindow) {
      logicArguments.push('--start-maximized');
    } else if (options.screenQuadrant !== undefined) {
      logicArguments.push('--screenQuadrant', String(options.screenQuadrant));
    }
    if (options.remoteDebuggingPort !== undefined) {
      logicArguments.push(`--remote-debugging-port=${options.remoteDebuggingPort}`);
    }
    logic = spawn(runtime.executable, logicArguments, {
      cwd: stateRoot,
      env: logicProcessEnvironment(),
      stdio: 'inherit',
    });
    if (options.maximizeWindow) maximizeLogicWindow(logic.pid);
    const exit = await waitForExit(logic);
    console.log(`[logic2-bridge] Logic exited (${exit.signal || exit.code})`);
    if (!stopping && exit.code) process.exitCode = exit.code;
  } finally {
    await controller?.shutdown();
    await proxy?.close();
    if (host.exitCode === null) {
      host.kill('SIGTERM');
      await Promise.race([
        waitForExit(host),
        new Promise(resolve => setTimeout(resolve, 3000)),
      ]);
    }
    graphLogMonitor.poll();
    graphLogMonitor.stop();
  }
}

module.exports = {
  installedAppCandidates,
  isLogicAppBundle,
  logicAppCandidatesFromPath,
  loadRuntimeCompatibilityProfiles,
  logicProcessEnvironment,
  macMaximizeScript,
  nativeGraphStartupTimeoutMs,
  nativeHookArguments,
  parseArguments,
  parseInjectionStats,
  readAppVersion,
  resolveAppPath,
  resolveRuntime,
  resolveRuntimeVersion,
  waitForNativeGraph,
  windowsMaximizeScript,
};

if (require.main === module) {
  main().catch(error => {
    console.error(`[logic2-bridge] fatal: ${error.message}`);
    process.exitCode = 1;
  });
}
