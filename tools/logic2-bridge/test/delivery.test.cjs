'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const path = require('node:path');
const {
  buildChecks,
  overallStatus,
  parseArguments,
  versionCheck,
} = require('../scripts/verify-delivery.cjs');

test('delivery gate covers every bridge runtime layer', () => {
  assert.deepEqual(
    buildChecks().map(check => check.id),
    [
      'bridge-node-check',
      'bridge-node-tests',
      'pxlogic-rust-format',
      'pxlogic-core-tests',
      'pxlogic-helper-check',
      'tauri-rust-format',
      'tauri-rust-tests',
    ],
  );
});

test('delivery gate fails only on a failed required check', () => {
  assert.equal(overallStatus([{ status: 'PASS' }, { status: 'WARN' }]), 'WARN');
  assert.equal(overallStatus([{ status: 'PASS' }, { status: 'FAIL' }]), 'FAIL');
  assert.equal(overallStatus([{ status: 'PASS' }]), 'PASS');
});

test('delivery report path is explicit and version drift remains visible', () => {
  const options = parseArguments(['--', '--report', './delivery-report.json']);
  assert.equal(options.reportPath, path.resolve('./delivery-report.json'));
  assert.throws(() => parseArguments(['--report']), /requires a file path/);
  const versions = versionCheck();
  assert.match(versions.status, /^(PASS|WARN)$/);
  assert.equal(typeof versions.versions.tauriConfig, 'string');
});
