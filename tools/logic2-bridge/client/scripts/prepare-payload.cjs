'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const repositoryRoot = path.resolve(__dirname, '..', '..', '..', '..');
const bridgeRoot = path.join(repositoryRoot, 'tools', 'logic2-bridge');
const payloadRoot = path.join(bridgeRoot, 'build', 'payload');

function windowsHostCompiler() {
  const toolsRoot = process.env.VCToolsInstallDir;
  if (!toolsRoot) return 'cl.exe';
  return path.join(toolsRoot, 'bin', 'Hostx64', 'x64', 'cl.exe');
}

const target = process.env.TAURI_TARGET || process.env.TARGET || (() => {
  if (process.platform === 'darwin') {
    return process.arch === 'arm64' ? 'aarch64-apple-darwin' : 'x86_64-apple-darwin';
  }
  if (process.platform === 'linux') return 'x86_64-unknown-linux-gnu';
  if (process.platform === 'win32') return 'x86_64-pc-windows-msvc';
  return null;
})();

const targetSettings = {
  'aarch64-apple-darwin': {
    helper: 'usb_smoke',
    nativeHost: 'graph-host',
    hostCompiler: 'xcrun',
    hostArgs: ['clang'],
    architecture: 'arm64',
  },
  'x86_64-apple-darwin': {
    helper: 'usb_smoke',
    nativeHost: 'graph-host',
    hostCompiler: 'xcrun',
    hostArgs: ['clang'],
    architecture: 'x86_64',
  },
  'x86_64-unknown-linux-gnu': {
    helper: 'usb_smoke',
    nativeHost: 'graph-host',
    hostCompiler: 'cc',
    hostArgs: [],
    architecture: 'x86-64',
  },
  'x86_64-pc-windows-msvc': {
    helper: 'usb_smoke.exe',
    nativeHost: 'graph-host.exe',
    hostCompiler: windowsHostCompiler(),
    hostArgs: [],
    architecture: 'x64',
  },
}[target];

if (!targetSettings) {
  throw new Error(`Unsupported bridge payload target: ${target || 'unknown'}`);
}

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: repositoryRoot,
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  if (result.status !== 0) {
    const failure = result.error
      ? `${result.error.code || result.error.name}: ${result.error.message}`
      : (result.stderr || result.stdout || `process exited with status ${result.status}`);
    throw new Error(
      `${command} ${args.join(' ')} failed:\n${failure}`,
    );
  }
  if (result.stdout) process.stdout.write(result.stdout);
  if (result.stderr) process.stderr.write(result.stderr);
}

const helperSource = path.join(
  repositoryRoot,
  'target',
  target,
  'release',
  targetSettings.helper,
);
const helperDestination = path.join(payloadRoot, targetSettings.helper);
const nativeSource = path.join(bridgeRoot, 'native', 'graph-host.c');
const nativeDestination = targetSettings.nativeHost
  ? path.join(payloadRoot, targetSettings.nativeHost)
  : null;

run('cargo', [
  'build',
  '--release',
  '--bin',
  'usb_smoke',
  '--target',
  target,
]);

fs.mkdirSync(payloadRoot, { recursive: true });
if (!fs.existsSync(helperSource)) {
  throw new Error(`PXLogic helper was not built: ${helperSource}`);
}
fs.copyFileSync(helperSource, helperDestination);
if (process.platform !== 'win32') fs.chmodSync(helperDestination, 0o755);

if (targetSettings.hostCompiler) {
  const hostArgs = target.startsWith('x86_64-pc-windows')
    ? [
      '/nologo',
      '/std:c11',
      '/experimental:c11atomics',
      '/O2',
      '/W4',
      '/D_CRT_SECURE_NO_WARNINGS',
      nativeSource,
      `/Fe:${nativeDestination}`,
      `/Fo:${path.join(payloadRoot, 'graph-host.obj')}`,
    ]
    : [
      ...targetSettings.hostArgs,
      '-std=c11',
      '-O2',
      '-Wall',
      '-Wextra',
      nativeSource,
      '-o',
      nativeDestination,
    ];
  if (target.startsWith('x86_64-unknown-linux')) hostArgs.push('-ldl', '-pthread');
  run(targetSettings.hostCompiler, hostArgs);
  fs.rmSync(path.join(payloadRoot, 'graph-host.obj'), { force: true });
  if (process.platform !== 'win32') fs.chmodSync(nativeDestination, 0o755);
  fs.copyFileSync(nativeDestination, path.join(bridgeRoot, 'build', targetSettings.nativeHost));
  if (process.platform !== 'win32') {
    fs.chmodSync(path.join(bridgeRoot, 'build', targetSettings.nativeHost), 0o755);
  }
}


const helperHelp = spawnSync(helperDestination, ['--help'], { encoding: 'utf8' });
if (helperHelp.status !== 0 ||
    !helperHelp.stdout.includes('--live-cross-only') ||
    !helperHelp.stdout.includes('--list-json')) {
  throw new Error(
    `${helperDestination} does not implement the bridge live capture and device discovery contract`,
  );
}

if (nativeDestination && process.platform !== 'win32') {
  const architecture = spawnSync('file', [nativeDestination], { encoding: 'utf8' });
  if (architecture.status !== 0 || !architecture.stdout.toLowerCase().includes(targetSettings.architecture.toLowerCase())) {
    throw new Error(`${nativeDestination} is not a ${targetSettings.architecture} executable`);
  }
}

console.log(`[logic2-bridge-client] prepared ${target} payload in ${payloadRoot}`);
