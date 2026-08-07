'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  METHOD_NAME,
  METHOD_SIGNATURE,
  locateWindowsOnDataBuffer,
} = require('../scripts/locate-windows-on-data-buffer.cjs');

function writeSection(buffer, offset, name, virtualSize, virtualAddress, rawSize, rawOffset) {
  buffer.write(name, offset, 'ascii');
  buffer.writeUInt32LE(virtualSize, offset + 8);
  buffer.writeUInt32LE(virtualAddress, offset + 12);
  buffer.writeUInt32LE(rawSize, offset + 16);
  buffer.writeUInt32LE(rawOffset, offset + 20);
}

function writeRipReference(buffer, rawOffset, fieldRva, targetRva) {
  buffer.writeUInt32LE(targetRva - (fieldRva + 4), rawOffset);
}

function syntheticGraphServer() {
  const buffer = Buffer.alloc(0x1000);
  buffer.write('MZ', 0, 'ascii');
  buffer.writeUInt32LE(0x80, 0x3c);
  buffer.write('PE\0\0', 0x80, 'ascii');
  buffer.writeUInt16LE(0x8664, 0x84);
  buffer.writeUInt16LE(3, 0x86);
  buffer.writeUInt16LE(0xf0, 0x94);
  writeSection(buffer, 0x188, '.text', 0x200, 0x1000, 0x200, 0x400);
  writeSection(buffer, 0x1b0, '.rdata', 0x300, 0x2000, 0x300, 0x600);
  writeSection(buffer, 0x1d8, '.pdata', 0x200, 0x3000, 0x200, 0x900);

  const functionRva = 0x1040;
  const functionRaw = 0x440;
  Buffer.from('488bc448895810488970204c89401855', 'hex').copy(buffer, functionRaw);
  const nameRaw = 0x620;
  const signatureRaw = 0x700;
  buffer.write(METHOD_NAME, nameRaw, 'ascii');
  buffer.write(METHOD_SIGNATURE, signatureRaw, 'ascii');
  writeRipReference(buffer, functionRaw + 0x43, functionRva + 0x43, 0x2020);
  writeRipReference(buffer, functionRaw + 0x83, functionRva + 0x83, 0x2100);
  buffer.writeUInt32LE(functionRva, 0x900);
  buffer.writeUInt32LE(0x1100, 0x904);
  buffer.writeUInt32LE(0x3050, 0x908);
  return buffer;
}

test('locates Windows OnDataBuffer through strings, RIP references, and pdata', () => {
  assert.deepEqual(locateWindowsOnDataBuffer(syntheticGraphServer()), {
    onDataBufferOffset: '0x1040',
    endOffset: '0x1100',
    unwindOffset: '0x3050',
    prologueHex: '488bc448895810488970204c89401855',
    evidence: {
      methodNameRvas: ['0x2020', '0x210d'],
      methodSignatureRvas: ['0x2100'],
      references: [
        { fieldRva: '0x1083', targetRva: '0x2020' },
        { fieldRva: '0x10c3', targetRva: '0x2100' },
      ],
    },
  });
});

test('rejects binaries without converging OnDataBuffer evidence', () => {
  const buffer = syntheticGraphServer();
  buffer.fill(0, 0x483, 0x487);
  buffer.fill(0, 0x4c3, 0x4c7);
  assert.throws(
    () => locateWindowsOnDataBuffer(buffer),
    /did not converge on one PE runtime function/,
  );
});
