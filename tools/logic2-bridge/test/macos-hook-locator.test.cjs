'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  inspectArm64Prologue,
  locateMacosOnDataBuffer,
  parseFunctionStarts,
  parseMacho64,
} = require('../lib/macos-hook-locator.cjs');

function writeFixed(buffer, offset, value, length = 16) {
  buffer.fill(0, offset, offset + length);
  buffer.write(value, offset, Math.min(length, Buffer.byteLength(value)), 'ascii');
}

function encodeAddImmediate(destination, source, immediate) {
  return (0x91000000 | (immediate << 10) | (source << 5) | destination) >>> 0;
}

function syntheticMacho() {
  const buffer = Buffer.alloc(0x1000);
  buffer.writeUInt32LE(0xfeedfacf, 0);
  buffer.writeUInt32LE(0x0100000c, 4);
  buffer.writeUInt32LE(2, 16);
  buffer.writeUInt32LE(248, 20);

  const segment = 32;
  buffer.writeUInt32LE(0x19, segment);
  buffer.writeUInt32LE(232, segment + 4);
  writeFixed(buffer, segment + 8, '__TEXT');
  buffer.writeBigUInt64LE(0n, segment + 24);
  buffer.writeBigUInt64LE(BigInt(buffer.length), segment + 32);
  buffer.writeBigUInt64LE(0n, segment + 40);
  buffer.writeBigUInt64LE(BigInt(buffer.length), segment + 48);
  buffer.writeUInt32LE(2, segment + 64);

  const text = segment + 72;
  writeFixed(buffer, text, '__text');
  writeFixed(buffer, text + 16, '__TEXT');
  buffer.writeBigUInt64LE(0x200n, text + 32);
  buffer.writeBigUInt64LE(0x200n, text + 40);
  buffer.writeUInt32LE(0x200, text + 48);

  const cstring = text + 80;
  writeFixed(buffer, cstring, '__cstring');
  writeFixed(buffer, cstring + 16, '__TEXT');
  buffer.writeBigUInt64LE(0x600n, cstring + 32);
  buffer.writeBigUInt64LE(0x200n, cstring + 40);
  buffer.writeUInt32LE(0x600, cstring + 48);

  const starts = segment + 232;
  buffer.writeUInt32LE(0x26, starts);
  buffer.writeUInt32LE(16, starts + 4);
  buffer.writeUInt32LE(0x900, starts + 8);
  buffer.writeUInt32LE(16, starts + 12);
  Buffer.from([0x80, 0x04, 0x80, 0x02, 0]).copy(buffer, 0x900);

  Buffer.from('ff8303d1f6570ba9f44f0ca9fd7b0da9', 'hex').copy(buffer, 0x200);
  buffer.writeUInt32LE(0xaa0303f4, 0x220);
  buffer.writeUInt32LE(0xfd400860, 0x224);
  buffer.writeUInt32LE(0x90000008, 0x240);
  buffer.writeUInt32LE(encodeAddImmediate(8, 8, 0x600), 0x244);
  buffer.writeUInt32LE(0x90000008, 0x248);
  buffer.writeUInt32LE(encodeAddImmediate(8, 8, 0x620), 0x24c);
  buffer.write('OnDataBuffer\0', 0x600, 'ascii');
  buffer.write('/src/logic_device_node.cpp\0', 0x620, 'ascii');
  return buffer;
}

test('locates a unique Mach-O OnDataBuffer function with a safe ARM64 entry', () => {
  const located = locateMacosOnDataBuffer(syntheticMacho());
  assert.equal(located.onDataBufferOffset, '0x200');
  assert.equal(located.prologueHex, 'ff8303d1f6570ba9f44f0ca9fd7b0da9');
  assert.equal(located.evidence.functionSize, 0x100);
  assert.equal(located.evidence.bufferArgumentMoves[0].destinationRegister, 'x20');
  assert.equal(located.evidence.bufferSizeLoads[0].instructionOffset, '0x24');
});

test('rejects a PC-relative instruction inside the ARM64 patch window', () => {
  const buffer = syntheticMacho();
  buffer.writeUInt32LE(0x90000008, 0x200);
  const macho = parseMacho64(buffer);
  assert.deepEqual(parseFunctionStarts(buffer, macho), [0x200, 0x300]);
  assert.throws(
    () => inspectArm64Prologue(buffer, macho, 0x200),
    /not trampoline-safe: ADR\/ADRP/,
  );
});
