'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const net = require('node:net');
const { PassThrough } = require('node:stream');
const {
  logicProcessEnvironment,
  macMaximizeScript,
  nativeGraphStartupTimeoutMs,
  nativeHookArguments,
  parseArguments,
  resolveRuntimeVersion,
  waitForNativeGraph,
  windowsMaximizeScript,
} = require('../index.cjs');

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

test('requires an explicit opt-in for pending live-validation profiles', () => {
  assert.equal(parseArguments(['--allow-pending-profile']).allowPendingProfile, true);
  assert.equal(parseArguments([]).allowPendingProfile, false);
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
