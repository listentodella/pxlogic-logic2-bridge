'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  REQUIRED_NOTARIZATION_ENV,
  main,
  requiredMissingEnvironment,
} = require('../scripts/verify-macos-release.cjs');

test('requires all credentials before notarized macOS distribution', () => {
  const missing = requiredMissingEnvironment({ APPLE_ID: 'developer@example.com' });
  assert.deepEqual(missing, REQUIRED_NOTARIZATION_ENV.filter(name => name !== 'APPLE_ID'));
  assert.deepEqual(requiredMissingEnvironment(Object.fromEntries(
    REQUIRED_NOTARIZATION_ENV.map(name => [name, 'configured']),
  )), []);
});

test('adhoc mode can validate release metadata without a bundle path', () => {
  assert.doesNotThrow(() => main(['--mode', 'adhoc'], {}));
});

test('notarized mode fails before signing when credentials are incomplete', () => {
  assert.throws(
    () => main(['--mode', 'notarized'], {}),
    /Notarized macOS release is missing:/,
  );
});

test('rejects an incomplete bundle argument', () => {
  assert.throws(
    () => main(['--mode', 'adhoc', '--bundle'], {}),
    /--bundle requires a macOS application bundle path/,
  );
});
