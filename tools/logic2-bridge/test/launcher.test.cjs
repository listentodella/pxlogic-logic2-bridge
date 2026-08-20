'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const { PassThrough } = require('node:stream');
const {
  isLogicAppBundle,
  logicAppCandidatesFromPath,
  logicProcessEnvironment,
  loadRuntimeCompatibilityProfiles,
  macMaximizeScript,
  nativeGraphStartupTimeoutMs,
  nativeHookArguments,
  parseArguments,
  parseInjectionStats,
  resolveRuntimeVersion,
  waitForNativeGraph,
  windowsMaximizeScript,
} = require('../index.cjs');

test('discovers a Logic app inside a selected macOS Applications folder', t => {
  if (process.platform !== 'darwin') return;
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'logic2-app-picker-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const logic = path.join(directory, 'Saleae Logic.app');
  const other = path.join(directory, 'Other.app');
  fs.mkdirSync(path.join(logic, 'Contents'), { recursive: true });
  fs.mkdirSync(path.join(other, 'Contents'), { recursive: true });
  fs.writeFileSync(
    path.join(logic, 'Contents', 'Info.plist'),
    `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.saleae.saleae</string></dict></plist>`,
  );
  fs.writeFileSync(
    path.join(other, 'Contents', 'Info.plist'),
    '<plist version="1.0"><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>',
  );
  assert.deepEqual(logicAppCandidatesFromPath(directory), [logic]);
  assert.equal(isLogicAppBundle(logic), true);
  assert.equal(isLogicAppBundle(other), false);
});

test('parses native injection quality counters', () => {
  assert.deepEqual(
    parseInjectionStats(
      '[logic2-bridge:inject] callback=128 buffer=65536 injected=65536 ' +
      'queued=131072 total=8388608 underflows=2 dropped=4096',
    ),
    {
      type: 'injection-progress',
      callbackCount: 128,
      callbackBufferBytes: 65536,
      callbackInjectedBytes: 65536,
      queuedBytes: 131072,
      injectedBytes: 8388608,
      underflows: 2,
      droppedBytes: 4096,
    },
  );
  assert.equal(parseInjectionStats('unrelated output'), null);
});

test('uses an automatically allocated public port by default', () => {
  assert.equal(parseArguments([]).port, 0);
  assert.equal(parseArguments(['--port', 'auto']).port, 0);
  assert.equal(parseArguments([]).maximizeWindow, true);
  assert.equal(parseArguments(['--screen-quadrant', '2']).maximizeWindow, false);
});

test('sizes the launched Logic window to the macOS visible desktop', () => {
  const script = macMaximizeScript(4321);
  assert.match(script, /NSScreen\.mainScreen/);
  assert.match(script, /unixId: pid/);
  assert.match(script, /windows\[0\]\.size/);
});

test('accepts a preferred public port', () => {
  assert.equal(parseArguments(['--port', '12472']).port, 12472);
  assert.throws(() => parseArguments(['--port', '65536']), /Invalid port/);
});

test('accepts an explicit PXLogic voltage threshold', () => {
  assert.equal(
    parseArguments(['--hardware-threshold-volts', '1.12']).hardwareThresholdVolts,
    1.12,
  );
  assert.equal(
    parseArguments(['--hardware-threshold-volts', '6.668']).hardwareThresholdVolts,
    6.668,
  );
  assert.throws(
    () => parseArguments(['--hardware-threshold-volts', '6.669']),
    /Invalid PXLogic hardware threshold/,
  );
});

test('requires an explicit opt-in for pending live-validation profiles', () => {
  assert.equal(parseArguments(['--allow-pending-profile']).allowPendingProfile, true);
  assert.equal(parseArguments([]).allowPendingProfile, false);
});

test('accepts an offline compatibility profile manifest', () => {
  const options = parseArguments(['--compatibility-profiles', './local-profiles.json']);
  assert.equal(options.compatibilityProfiles, path.resolve('./local-profiles.json'));
});

test('rejects stale local profiles before the bridge runtime loads them', t => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'logic2-stale-profile-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const profilesPath = path.join(directory, 'compatibility-analysis.json');
  fs.writeFileSync(profilesPath, JSON.stringify({
    schemaVersion: 1,
    analyzerVersion: 0,
    profiles: [],
  }));
  assert.throws(
    () => loadRuntimeCompatibilityProfiles(profilesPath),
    /analyzer version 0 does not match 2/,
  );
});

test('uses exact profile metadata only when installation metadata is unavailable', () => {
  const profile = { logicVersion: '2.4.46' };
  assert.equal(resolveRuntimeVersion('2.5.0', profile), '2.5.0');
  assert.equal(resolveRuntimeVersion(undefined, profile), '2.4.46');
  assert.equal(resolveRuntimeVersion(undefined, null), 'unknown');
});

test('does not force the official Electron app into Node mode', () => {
  const environment = logicProcessEnvironment({
    ELECTRON_RUN_AS_NODE: '1',
    PATH: '/usr/bin',
  });
  assert.deepEqual(environment, { PATH: '/usr/bin' });
});

test('waits for the launched Logic window before maximizing it on Windows', () => {
  const script = windowsMaximizeScript(4321);
  assert.match(script, /Get-Process -Id 4321/);
  assert.match(script, /MainWindowHandle/);
  assert.match(script, /ShowWindowAsync\([^\n]+, 3\)/);
});

test('accepts a bounded native GraphServer startup timeout override', () => {
  assert.equal(nativeGraphStartupTimeoutMs({ PXLOGIC_GRAPH_START_TIMEOUT_MS: '45000' }), 45000);
  assert.equal(
    nativeGraphStartupTimeoutMs({ PXLOGIC_GRAPH_START_TIMEOUT_MS: 'invalid' }),
    process.platform === 'win32' ? 60000 : 15000,
  );
  assert.equal(
    nativeGraphStartupTimeoutMs({ PXLOGIC_GRAPH_START_TIMEOUT_MS: '999999' }),
    process.platform === 'win32' ? 60000 : 15000,
  );
});

test('passes a matched profile to the data-driven native host', () => {
  assert.deepEqual(nativeHookArguments({
    compatibility: {
      profile: {
        id: 'logic-test',
        graph: { identity: 'ABCDEF01' },
        hook: {
          onDataBufferOffset: '0x1234',
          prologueHex: '554889e5',
        },
      },
    },
  }), ['logic-test', 'ABCDEF01', '0x1234', '554889e5']);
});

test('accepts a listening GraphServer when CreateGraphServer has not returned', async t => {
  const server = net.createServer(socket => socket.destroy());
  await new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  t.after(() => server.close());
  const address = server.address();
  assert.equal(typeof address, 'object');

  const host = new EventEmitter();
  host.stdout = new PassThrough();
  const port = await waitForNativeGraph(host, address.port, 1000);
  assert.equal(port, address.port);
});

test('accepts an auto-assigned native GraphServer port', async () => {
  const host = new EventEmitter();
  host.stdout = new PassThrough();
  const ready = waitForNativeGraph(host, 0, 1000);
  host.stdout.write('GRAPH_WS_READY ws://127.0.0.1:49152/saleae\n');
  assert.equal(await ready, 49152);
});
