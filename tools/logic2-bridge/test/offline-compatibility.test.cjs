'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {
  inspectGraphBinary,
  loadCompatibilityProfiles,
} = require('../lib/compatibility.cjs');
const {
  ANALYZER_VERSION,
  analyzeGraph,
  fileOffsetToRuntimeOffset,
  loadLocalManifest,
  matchKnownTrampolinePrologue,
} = require('../lib/offline-compatibility.cjs');

const LOCATOR_SIGNATURE = Buffer.from(
  'ff8303d1f6570ba9f44f0ca9fd7b0da9fd43039108a0503908060034f40303aa',
  'hex',
);

function syntheticMacho() {
  const buffer = Buffer.alloc(0x400);
  buffer.writeUInt32LE(0xfeedfacf, 0);
  buffer.writeUInt32LE(0x0100000c, 4);
  buffer.writeUInt32LE(2, 16);
  buffer.writeUInt32LE(96, 20);

  buffer.writeUInt32LE(0x1b, 32);
  buffer.writeUInt32LE(24, 36);
  Buffer.from('123456789abcdef00011223344556677', 'hex').copy(buffer, 40);

  const segment = 56;
  buffer.writeUInt32LE(0x19, segment);
  buffer.writeUInt32LE(72, segment + 4);
  buffer.write('__TEXT', segment + 8, 'ascii');
  buffer.writeBigUInt64LE(0n, segment + 24);
  buffer.writeBigUInt64LE(BigInt(buffer.length), segment + 32);
  buffer.writeBigUInt64LE(0n, segment + 40);
  buffer.writeBigUInt64LE(BigInt(buffer.length), segment + 48);
  LOCATOR_SIGNATURE.copy(buffer, 0x200);
  return buffer;
}

function referenceProfile() {
  return {
    id: 'known-reference',
    logicVersion: '2.4.46',
    platform: 'darwin',
    architecture: 'arm64',
    graph: {
      identityKind: 'macho-lc-uuid',
      identity: 'AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE',
      sha256: '0'.repeat(64),
    },
    hook: {
      status: 'verified',
      onDataBufferOffset: '0x100',
      prologueHex: LOCATOR_SIGNATURE.subarray(0, 16).toString('hex'),
      locatorSignatureHex: LOCATOR_SIGNATURE.toString('hex'),
      validation: 'synthetic reference',
    },
  };
}

test('maps a Mach-O file offset through LC_SEGMENT_64', () => {
  assert.equal(fileOffsetToRuntimeOffset(syntheticMacho(), 'mach-o', 0x200), 0x200);
});

test('accepts only a maintained trampoline-safe Windows prologue', () => {
  const profile = {
    ...referenceProfile(),
    platform: 'win32',
    architecture: 'x64',
    hook: {
      ...referenceProfile().hook,
      prologueHex: '488bc448895810488970204c89401855',
    },
  };
  assert.equal(
    matchKnownTrampolinePrologue(
      { prologueHex: profile.hook.prologueHex },
      'win32',
      'x64',
      [profile],
    ).id,
    profile.id,
  );
  assert.throws(
    () => matchKnownTrampolinePrologue(
      { prologueHex: '554889e54883ec2048897df8488975f0' },
      'win32',
      'x64',
      [profile],
    ),
    /trampoline-safe prologue/,
  );
});

test('creates and reuses an exact-fingerprint offline candidate', t => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'pxlogic-offline-profile-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const graphPath = path.join(directory, 'libgraph_server_shared.dylib');
  const cachePath = path.join(directory, 'compatibility-analysis.json');
  fs.writeFileSync(graphPath, syntheticMacho());

  const first = analyzeGraph({
    graphPath,
    logicVersion: '2.5.0',
    platform: 'darwin',
    architecture: 'arm64',
    cachePath,
    profiles: [referenceProfile()],
  });
  assert.equal(first.status, 'candidate');
  assert.equal(first.cached, false);
  assert.equal(first.profile.hook.onDataBufferOffset, '0x200');
  assert.equal(first.profile.hook.status, 'candidate');
  assert.equal(first.profile.analysis.analyzerVersion, ANALYZER_VERSION);

  const second = analyzeGraph({
    graphPath,
    logicVersion: '2.5.0',
    platform: 'darwin',
    architecture: 'arm64',
    cachePath,
    profiles: [referenceProfile()],
  });
  assert.equal(second.status, 'candidate');
  assert.equal(second.cached, true);
  assert.equal(loadLocalManifest(cachePath).profiles.length, 1);
  assert.equal(loadCompatibilityProfiles(cachePath)[0].id, first.profile.id);
});

test('prefers an exact built-in profile and retries stale analyzer cache records', t => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'pxlogic-offline-maintenance-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const graphPath = path.join(directory, 'libgraph_server_shared.dylib');
  const cachePath = path.join(directory, 'compatibility-analysis.json');
  fs.writeFileSync(graphPath, syntheticMacho());
  const fingerprint = inspectGraphBinary(graphPath);
  const known = referenceProfile();
  known.graph = {
    identityKind: fingerprint.identityKind,
    identity: fingerprint.identity,
    sha256: fingerprint.sha256,
  };

  const exact = analyzeGraph({
    graphPath,
    logicVersion: '2.5.0',
    platform: 'darwin',
    architecture: 'arm64',
    cachePath,
    profiles: [known],
  });
  assert.equal(exact.status, 'known');
  assert.equal(fs.existsSync(cachePath), false);

  fs.writeFileSync(cachePath, JSON.stringify({
    schemaVersion: 1,
    analyzerVersion: 0,
    profiles: [{
      ...referenceProfile(),
      graph: {
        identityKind: fingerprint.identityKind,
        identity: fingerprint.identity,
        sha256: fingerprint.sha256,
      },
      analysis: { analyzerVersion: 0 },
    }],
    failures: [{
      logicVersion: '2.5.0',
      platform: 'darwin',
      architecture: 'arm64',
      graph: {
        identityKind: fingerprint.identityKind,
        identity: fingerprint.identity,
        sha256: fingerprint.sha256,
      },
      analyzerVersion: 0,
      reason: 'stale failure',
    }],
  }));
  known.graph.sha256 = '0'.repeat(64);
  const retried = analyzeGraph({
    graphPath,
    logicVersion: '2.5.0',
    platform: 'darwin',
    architecture: 'arm64',
    cachePath,
    profiles: [known],
  });
  assert.equal(retried.status, 'candidate');
  assert.equal(retried.cached, false);
  assert.equal(loadLocalManifest(cachePath).profiles.length, 1);
  assert.equal(loadLocalManifest(cachePath).profiles[0].analysis.analyzerVersion, ANALYZER_VERSION);
  assert.equal(loadLocalManifest(cachePath).failures.length, 0);
});

test('caches an ambiguous offline analysis as unsupported', t => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'pxlogic-offline-failure-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const graphPath = path.join(directory, 'libgraph_server_shared.dylib');
  const cachePath = path.join(directory, 'compatibility-analysis.json');
  const buffer = syntheticMacho();
  LOCATOR_SIGNATURE.copy(buffer, 0x280);
  fs.writeFileSync(graphPath, buffer);

  const result = analyzeGraph({
    graphPath,
    logicVersion: '2.5.0',
    platform: 'darwin',
    architecture: 'arm64',
    cachePath,
    profiles: [referenceProfile()],
  });
  assert.equal(result.status, 'unsupported');
  assert.match(result.reason, /did not resolve to one candidate/);
  const manifest = loadLocalManifest(cachePath);
  assert.equal(manifest.profiles.length, 0);
  assert.equal(manifest.failures.length, 1);
  assert.equal(manifest.failures[0].analyzerVersion, ANALYZER_VERSION);
});
