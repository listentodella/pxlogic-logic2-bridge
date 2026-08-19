'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
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

test('experimental profile launch keeps an explicit one-shot confirmation contract', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');

  assert.match(html, /id="experimental-confirmation"/);
  assert.match(html, /id="experimental-confirmation-checkbox"/);
  assert.match(html, /id="continue-experimental-button"[^>]*disabled/);
  assert.match(app, /let experimentalConfirmationToken = null;/);
  assert.match(app, /phase: currentState\.phase/);
  assert.match(app, /experimentalConfirmationToken\.confirmed = true/);
  assert.match(app, /!elements\.experimentalConfirmationCheckbox\.checked/);
  assert.match(app, /function requestExperimentalConfirmation\(\)/);
  assert.match(app, /function consumeExperimentalConfirmationFingerprint\(\)/);
  assert.match(app, /pendingProfileFingerprint,/);
});
