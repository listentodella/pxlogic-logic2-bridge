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
  assert.match(app, /if \(elements\.experimentalConfirmation\.open\) elements\.experimentalConfirmation\.close\(\)/);
  assert.match(app, /function requestExperimentalConfirmation\(\)/);
  assert.match(app, /function consumeExperimentalConfirmationFingerprint\(\)/);
  assert.match(app, /pendingProfileFingerprint,/);
});

test('firmware picker defaults to the latest image and confirms a downgrade', () => {
  const rendererRoot = path.resolve(__dirname, '../client/renderer');
  const html = fs.readFileSync(path.join(rendererRoot, 'index.html'), 'utf8');
  const app = fs.readFileSync(path.join(rendererRoot, 'app.js'), 'utf8');

  assert.match(html, /id="pxlogic-firmware-version"/);
  assert.match(html, /id="firmware-downgrade-warning"[^>]*hidden/);
  assert.match(html, /id="firmware-downgrade-checkbox"/);
  assert.match(html, /id="confirm-firmware-downgrade-button"[^>]*disabled/);

  // An unknown or missing stored selection resolves to the image flagged latest.
  assert.match(app, /function latestFirmwareRelease\(\)/);
  assert.match(app, /findFirmwareRelease\(selectedId\) \|\| latest/);
  // Only a non-latest selection may open the confirmation dialog.
  assert.match(app, /if \(!release \|\| release\.latest \|\| release\.id === lastConfirmedFirmwareId\) return true;/);
  assert.match(app, /if \(!elements\.firmwareDowngradeCheckbox\.checked\) return;/);
  // Cancelling must restore the previous selection rather than leave the older
  // image staged for the next Bridge start.
  assert.match(app, /findFirmwareRelease\(lastConfirmedFirmwareId\) \|\| latestFirmwareRelease\(\)/);
  assert.match(app, /if \(accepted\) persistSettings\(\);/);
  assert.match(app, /pxlogicFirmwareId: elements\.pxlogicFirmwareVersion\.value,/);
});

test('every selectable firmware image is shipped and matches the manifest', () => {
  const firmwareRoot = path.resolve(__dirname, '../../../resources/firmware');
  const manifest = JSON.parse(fs.readFileSync(path.join(firmwareRoot, 'releases.json'), 'utf8'));

  assert.equal(manifest.schemaVersion, 1);
  const latest = manifest.releases.filter(release => release.latest);
  assert.equal(latest.length, 1, 'exactly one image may be marked latest');
  assert.equal(latest[0].id, manifest.default, 'the default selection must be the latest image');

  const crypto = require('node:crypto');
  for (const release of manifest.releases) {
    const image = fs.readFileSync(path.join(firmwareRoot, release.fileName));
    assert.equal(image.length, release.byteLength, `${release.fileName} length`);
    assert.equal(
      crypto.createHash('sha256').update(image).digest('hex'),
      release.sha256,
      `${release.fileName} digest`,
    );
  }
});
