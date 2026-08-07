'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  createInjectionConfig,
  encodePxlogicCrossChunk,
  normalizeEnabledChannels,
  reverseBits32,
} = require('../lib/logic-format.cjs');

test('normalizes sparse digital channel selection', () => {
  assert.deepEqual(normalizeEnabledChannels('5,1,5,0'), [0, 1, 5]);
  assert.throws(() => normalizeEnabledChannels(''), /one or more/);
  assert.throws(() => normalizeEnabledChannels('16'), /0 through 15/);
});

test('reverses PXLogic lane words into Logic callback words', () => {
  const input = Buffer.alloc(3 * 8);
  for (let lane = 0; lane < 3; lane += 1) {
    input.writeUInt32LE(0x01020304 + lane, lane * 8);
    input.writeUInt32LE(0x11223344 + lane, lane * 8 + 4);
  }
  const output = encodePxlogicCrossChunk(input, [0, 2, 5]);
  assert.equal(output.length, 3 * 2 * 4);
  assert.equal(output.readUInt32LE(0), reverseBits32(0x01020304));
  assert.equal(output.readUInt32LE(8), reverseBits32(0x01020306));
  assert.equal(output.readUInt32LE(12), reverseBits32(0x11223344));
});

test('emits the native injection configuration frame', () => {
  const frame = createInjectionConfig([0, 2, 5]);
  assert.equal(frame.subarray(0, 4).toString('ascii'), 'PXLI');
  assert.equal(frame[4], 1);
  assert.equal(frame.readUInt32LE(8), 4);
  assert.equal(frame.readUInt32LE(12), 24);
});
