'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {
  CAB_HEADER_BYTES,
  extractAdvancedInstallerCabinet,
  findAdvancedInstallerCabinet,
} = require('../scripts/extract-advanced-installer-cab.cjs');

function fakeInstaller({ cabinetOffset = 700, cabinetSize = 900 } = {}) {
  const installer = Buffer.alloc(cabinetOffset + cabinetSize + 100, 0x5a);
  const cabinet = Buffer.alloc(cabinetSize, 0xa5);
  cabinet.write('MSCF', 0, 'ascii');
  cabinet.writeUInt32LE(cabinetSize, 8);
  cabinet.writeUInt32LE(44, 16);
  cabinet[24] = 3;
  cabinet[25] = 1;
  cabinet.writeUInt16LE(1, 26);
  cabinet.writeUInt16LE(2, 28);
  for (let index = 0; index < CAB_HEADER_BYTES; index += 1) {
    cabinet[index] ^= 0xff;
  }
  cabinet.copy(installer, cabinetOffset);
  return { installer, cabinetOffset, cabinetSize };
}

test('finds and validates an Advanced Installer XOR-obfuscated CAB', () => {
  const fixture = fakeInstaller();
  const result = findAdvancedInstallerCabinet(fixture.installer);
  assert.equal(result.offset, fixture.cabinetOffset);
  assert.equal(result.size, fixture.cabinetSize);
  assert.equal(result.filesOffset, 44);
  assert.equal(result.version, '1.3');
  assert.equal(result.folderCount, 1);
  assert.equal(result.fileCount, 2);
});

test('extracts the CAB while decoding only its first 512 bytes', t => {
  const fixture = fakeInstaller();
  const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), 'logic2-cab-test-'));
  t.after(() => fs.rmSync(tempRoot, { recursive: true, force: true }));
  const installerPath = path.join(tempRoot, 'Logic.exe');
  const outputPath = path.join(tempRoot, 'payload', 'Logic.cab');
  fs.writeFileSync(installerPath, fixture.installer);

  const result = extractAdvancedInstallerCabinet(installerPath, outputPath);
  const extracted = fs.readFileSync(outputPath);
  assert.equal(result.offset, fixture.cabinetOffset);
  assert.equal(extracted.length, fixture.cabinetSize);
  assert.equal(extracted.toString('ascii', 0, 4), 'MSCF');
  assert.ok(extracted.subarray(CAB_HEADER_BYTES).equals(
    fixture.installer.subarray(
      fixture.cabinetOffset + CAB_HEADER_BYTES,
      fixture.cabinetOffset + fixture.cabinetSize,
    ),
  ));
});

test('rejects an inverted CAB signature with invalid bounds', () => {
  const fixture = fakeInstaller();
  const header = Buffer.from(fixture.installer.subarray(
    fixture.cabinetOffset,
    fixture.cabinetOffset + CAB_HEADER_BYTES,
  ));
  for (let index = 0; index < header.length; index += 1) header[index] ^= 0xff;
  header.writeUInt32LE(fixture.installer.length * 2, 8);
  for (let index = 0; index < header.length; index += 1) header[index] ^= 0xff;
  header.copy(fixture.installer, fixture.cabinetOffset);
  assert.throws(
    () => findAdvancedInstallerCabinet(fixture.installer),
    /No valid XOR-obfuscated Advanced Installer CAB/,
  );
});
