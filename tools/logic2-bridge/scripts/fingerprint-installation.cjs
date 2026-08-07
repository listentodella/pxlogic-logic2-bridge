#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const {
  findGraphServerBinary,
  inspectGraphBinary,
  loadCompatibilityProfiles,
  platformForBinaryFormat,
  readLogicVersionFromInstallation,
} = require('../lib/compatibility.cjs');

const installationPath = process.argv[2];
if (!installationPath || installationPath === '--help' || installationPath === '-h') {
  console.log(
    'Usage: node scripts/fingerprint-installation.cjs <path> ' +
    '[--platform darwin|linux|win32] [--architecture arm64|x64]',
  );
  process.exit(installationPath ? 0 : 2);
}

function option(name, fallback) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

let graphPath = findGraphServerBinary(installationPath);
if (!graphPath && fs.statSync(installationPath).isFile()) {
  const directFingerprint = inspectGraphBinary(path.resolve(installationPath));
  if (directFingerprint.format !== 'unknown') graphPath = path.resolve(installationPath);
}
if (!graphPath) {
  throw new Error(`GraphServer binary was not found below ${path.resolve(installationPath)}`);
}
const fingerprint = inspectGraphBinary(graphPath);
const platform = option(
  '--platform',
  platformForBinaryFormat(fingerprint.format) || process.platform,
);
const architecture = option('--architecture', fingerprint.architecture);
const logicVersion = readLogicVersionFromInstallation(installationPath);
const profiles = loadCompatibilityProfiles();
const matches = profiles.filter(profile =>
  (!logicVersion || profile.logicVersion === logicVersion) &&
  profile.platform === platform &&
  profile.architecture === architecture,
);
const profile = matches.find(candidate =>
  candidate.graph.identityKind === fingerprint.identityKind &&
  candidate.graph.identity.toLowerCase() === fingerprint.identity.toLowerCase() &&
  candidate.graph.sha256.toLowerCase() === fingerprint.sha256.toLowerCase(),
);

console.log(JSON.stringify({
  installationPath: path.resolve(installationPath),
  logicVersion,
  platform,
  architecture,
  graphPath,
  fingerprint,
  profile: profile?.id || null,
  hookStatus: profile?.hook.status || 'unknown',
}, null, 2));
