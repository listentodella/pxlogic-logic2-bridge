#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');

const REQUIRED_NOTARIZATION_ENV = [
  'APPLE_SIGNING_IDENTITY',
  'APPLE_ID',
  'APPLE_TEAM_ID',
  'APPLE_PASSWORD',
  'APPLE_CERTIFICATE',
  'APPLE_CERTIFICATE_PASSWORD',
];

function requiredMissingEnvironment(environment = process.env) {
  return REQUIRED_NOTARIZATION_ENV.filter(name => !String(environment[name] || '').trim());
}

function run(command, args) {
  const result = spawnSync(command, args, { encoding: 'utf8' });
  if (result.error) throw result.error;
  return {
    status: result.status,
    output: `${result.stdout || ''}${result.stderr || ''}`,
  };
}

function verifyBundle(bundlePath, mode) {
  const verification = run('codesign', ['--verify', '--deep', '--strict', '--verbose=2', bundlePath]);
  if (verification.status !== 0) {
    throw new Error(`codesign verification failed:\n${verification.output}`);
  }
  const details = run('codesign', ['-dv', '--verbose=4', bundlePath]);
  if (details.status !== 0) throw new Error(`codesign details failed:\n${details.output}`);
  if (mode === 'adhoc' && !details.output.includes('Signature=adhoc')) {
    throw new Error('Expected an ad-hoc macOS signature');
  }
  if (mode === 'notarized') {
    if (!details.output.includes('Authority=Developer ID Application:')) {
      throw new Error('Expected a Developer ID Application signature');
    }
    const assessment = run('spctl', ['--assess', '--type', 'execute', '--verbose=4', bundlePath]);
    if (assessment.status !== 0) {
      throw new Error(`Gatekeeper assessment failed:\n${assessment.output}`);
    }
    const stapler = run('xcrun', ['stapler', 'validate', bundlePath]);
    if (stapler.status !== 0) {
      throw new Error(`notarization ticket validation failed:\n${stapler.output}`);
    }
  }
}

function main(argv = process.argv.slice(2), environment = process.env) {
  const modeIndex = argv.indexOf('--mode');
  const mode = modeIndex >= 0 ? argv[modeIndex + 1] : 'adhoc';
  if (!['adhoc', 'notarized'].includes(mode)) {
    throw new Error(`Unknown macOS release mode: ${mode}`);
  }
  if (mode === 'notarized') {
    const missing = requiredMissingEnvironment(environment);
    if (missing.length) {
      throw new Error(`Notarized macOS release is missing: ${missing.join(', ')}`);
    }
  }
  const bundleIndex = argv.indexOf('--bundle');
  const bundle = bundleIndex >= 0 ? argv[bundleIndex + 1] : undefined;
  if (bundleIndex >= 0 && !bundle) {
    throw new Error('--bundle requires a macOS application bundle path');
  }
  if (bundle) verifyBundle(bundle, mode);
  console.log(`macOS release mode: ${mode}`);
}

module.exports = {
  REQUIRED_NOTARIZATION_ENV,
  main,
  requiredMissingEnvironment,
};

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`[release-check] ${error.message}`);
    process.exitCode = 1;
  }
}
