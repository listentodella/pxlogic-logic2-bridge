'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {
  loadCompatibilityProfiles,
  matchCompatibilityProfile,
  parseBinaryIdentity,
  platformForBinaryFormat,
} = require('../lib/compatibility.cjs');

test('maps native binary formats to compatibility platforms', () => {
  assert.equal(platformForBinaryFormat('mach-o'), 'darwin');
  assert.equal(platformForBinaryFormat('elf'), 'linux');
  assert.equal(platformForBinaryFormat('pe'), 'win32');
  assert.equal(platformForBinaryFormat('unknown'), null);
});

test('parses a Mach-O LC_UUID', () => {
  const buffer = Buffer.alloc(64);
  buffer.writeUInt32LE(0xfeedfacf, 0);
  buffer.writeUInt32LE(0x0100000c, 4);
  buffer.writeUInt32LE(1, 16);
  buffer.writeUInt32LE(24, 20);
  buffer.writeUInt32LE(0x1b, 32);
  buffer.writeUInt32LE(24, 36);
  Buffer.from('0df176318e043501a7b549a62e233fb0', 'hex').copy(buffer, 40);
  assert.deepEqual(parseBinaryIdentity(buffer), {
    format: 'mach-o',
    architecture: 'arm64',
    identityKind: 'macho-lc-uuid',
    identity: '0DF17631-8E04-3501-A7B5-49A62E233FB0',
  });
});

test('parses an ELF GNU Build ID', () => {
  const buffer = Buffer.alloc(160);
  Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1]).copy(buffer, 0);
  buffer.writeUInt16LE(62, 18);
  buffer.writeBigUInt64LE(64n, 32);
  buffer.writeUInt16LE(56, 54);
  buffer.writeUInt16LE(1, 56);
  buffer.writeUInt32LE(4, 64);
  buffer.writeBigUInt64LE(120n, 72);
  buffer.writeBigUInt64LE(28n, 96);
  buffer.writeUInt32LE(4, 120);
  buffer.writeUInt32LE(8, 124);
  buffer.writeUInt32LE(3, 128);
  buffer.write('GNU\0', 132, 'ascii');
  Buffer.from('f82e8683c47fcd77', 'hex').copy(buffer, 136);
  assert.deepEqual(parseBinaryIdentity(buffer), {
    format: 'elf',
    architecture: 'x64',
    identityKind: 'elf-gnu-build-id',
    identity: 'f82e8683c47fcd77',
  });
});

test('parses a PE CodeView GUID and age', () => {
  const buffer = Buffer.alloc(0x280);
  buffer.write('MZ', 0, 'ascii');
  buffer.writeUInt32LE(0x40, 0x3c);
  buffer.write('PE\0\0', 0x40, 'ascii');
  buffer.writeUInt16LE(0x8664, 0x44);
  buffer.writeUInt16LE(1, 0x46);
  buffer.writeUInt16LE(0xf0, 0x54);
  buffer.writeUInt16LE(0x20b, 0x58);
  buffer.writeUInt32LE(0x1000, 0x58 + 112 + 6 * 8);
  buffer.writeUInt32LE(28, 0x58 + 112 + 6 * 8 + 4);
  const section = 0x58 + 0xf0;
  buffer.writeUInt32LE(0x1000, section + 12);
  buffer.writeUInt32LE(0x1000, section + 16);
  buffer.writeUInt32LE(0x200, section + 20);
  const debug = 0x200;
  buffer.writeUInt32LE(2, debug + 12);
  buffer.writeUInt32LE(24, debug + 16);
  buffer.writeUInt32LE(0x240, debug + 24);
  buffer.write('RSDS', 0x240, 'ascii');
  Buffer.from('78563412bc9aefbe0011223344556677', 'hex').copy(buffer, 0x244);
  buffer.writeUInt32LE(7, 0x254);
  assert.deepEqual(parseBinaryIdentity(buffer), {
    format: 'pe',
    architecture: 'x64',
    identityKind: 'pe-codeview-guid-age',
    identity: '12345678-9ABC-BEEF-0011-223344556677-7',
    timestamp: 0,
  });
});

test('reuses an exact GraphServer binary profile across outer Logic versions', t => {
  const buffer = Buffer.alloc(0x280);
  buffer.write('MZ', 0, 'ascii');
  buffer.writeUInt32LE(0x40, 0x3c);
  buffer.write('PE\0\0', 0x40, 'ascii');
  buffer.writeUInt16LE(0x8664, 0x44);
  buffer.writeUInt16LE(1, 0x46);
  buffer.writeUInt16LE(0xf0, 0x54);
  buffer.writeUInt16LE(0x20b, 0x58);
  buffer.writeUInt32LE(0x1000, 0x58 + 112 + 6 * 8);
  buffer.writeUInt32LE(28, 0x58 + 112 + 6 * 8 + 4);
  const section = 0x58 + 0xf0;
  buffer.writeUInt32LE(0x1000, section + 12);
  buffer.writeUInt32LE(0x1000, section + 16);
  buffer.writeUInt32LE(0x200, section + 20);
  buffer.writeUInt32LE(2, 0x200 + 12);
  buffer.writeUInt32LE(24, 0x200 + 16);
  buffer.writeUInt32LE(0x240, 0x200 + 24);
  buffer.write('RSDS', 0x240, 'ascii');
  Buffer.from('78563412bc9aefbe0011223344556677', 'hex').copy(buffer, 0x244);
  buffer.writeUInt32LE(7, 0x254);

  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'logic2-profile-test-'));
  t.after(() => fs.rmSync(directory, { recursive: true, force: true }));
  const graphPath = path.join(directory, 'graph_server_shared.dll');
  fs.writeFileSync(graphPath, buffer);
  const sha256 = crypto.createHash('sha256').update(buffer).digest('hex');
  const result = matchCompatibilityProfile({
    logicVersion: '2.9.99',
    platform: 'win32',
    architecture: 'x64',
    graphPath,
    profiles: [{
      id: 'same-graph-different-app-version',
      logicVersion: '2.4.46',
      platform: 'win32',
      architecture: 'x64',
      graph: {
        identityKind: 'pe-codeview-guid-age',
        identity: '12345678-9ABC-BEEF-0011-223344556677-7',
        sha256,
      },
      hook: { status: 'verified' },
    }],
  });
  assert.equal(result.profile?.id, 'same-graph-different-app-version');
  assert.equal(result.supported, true);
});

test('keeps only exact GraphServer identities in the profile manifest', () => {
  const profiles = loadCompatibilityProfiles();
  assert.deepEqual(
    profiles.map(profile => [profile.platform, profile.hook.status]),
    [
      ['darwin', 'verified'],
      ['linux', 'pending-live-validation'],
      ['win32', 'pending-live-validation'],
    ],
  );
  for (const profile of profiles) {
    assert.match(profile.graph.sha256, /^[0-9a-f]{64}$/);
    if (profile.hook.prologueHex !== undefined) {
      assert.match(profile.hook.prologueHex, /^[0-9a-f]+$/);
    }
  }
});
