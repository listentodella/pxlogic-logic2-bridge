'use strict';

const INJECTION_FRAME = Object.freeze({
  CONFIG: 1,
  DATA: 2,
  END: 3,
});

function normalizeEnabledChannels(value) {
  const source = Array.isArray(value) ? value : String(value).split(',');
  const channels = [...new Set(source
    .map(channel => String(channel).trim())
    .filter(channel => channel.length > 0)
    .map(channel => Number(channel))
    .filter(Number.isInteger))]
    .sort((left, right) => left - right);
  if (channels.length === 0 || channels.some(channel => channel < 0 || channel > 15)) {
    throw new Error('Enabled channels must contain one or more unique indexes from 0 through 15');
  }
  return channels;
}

function reverseBits32(value) {
  value >>>= 0;
  value = (((value & 0x55555555) << 1) | ((value >>> 1) & 0x55555555)) >>> 0;
  value = (((value & 0x33333333) << 2) | ((value >>> 2) & 0x33333333)) >>> 0;
  value = (((value & 0x0f0f0f0f) << 4) | ((value >>> 4) & 0x0f0f0f0f)) >>> 0;
  value = (((value & 0x00ff00ff) << 8) | ((value >>> 8) & 0x00ff00ff)) >>> 0;
  return (((value & 0x0000ffff) << 16) | (value >>> 16)) >>> 0;
}

function encodePxlogicCrossChunk(pxlogicCrossData, enabledChannels) {
  const channels = normalizeEnabledChannels(enabledChannels);
  const stripeBytes = channels.length * 8;
  if (!Buffer.isBuffer(pxlogicCrossData) ||
      pxlogicCrossData.length === 0 ||
      pxlogicCrossData.length % stripeBytes !== 0) {
    throw new Error('PXLogic input is not aligned to a complete 64-sample stripe');
  }

  const stripeCount = pxlogicCrossData.length / stripeBytes;
  const output = Buffer.allocUnsafe(stripeCount * channels.length * 2 * 4);
  let outputOffset = 0;
  for (let stripeIndex = 0; stripeIndex < stripeCount; stripeIndex += 1) {
    const stripeOffset = stripeIndex * stripeBytes;
    for (let half = 0; half < 2; half += 1) {
      for (let laneIndex = 0; laneIndex < channels.length; laneIndex += 1) {
        const laneOffset = stripeOffset + laneIndex * 8 + half * 4;
        const word = reverseBits32(pxlogicCrossData.readUInt32LE(laneOffset));
        output.writeUInt32LE(word, outputOffset);
        outputOffset += 4;
      }
    }
  }
  return output;
}

function createInjectionFrame(type, payload = Buffer.alloc(0)) {
  const body = Buffer.isBuffer(payload) ? payload : Buffer.from(payload);
  const frame = Buffer.allocUnsafe(12 + body.length);
  frame.write('PXLI', 0, 4, 'ascii');
  frame[4] = type;
  frame.fill(0, 5, 8);
  frame.writeUInt32LE(body.length, 8);
  body.copy(frame, 12);
  return frame;
}

function createInjectionConfig(enabledChannels) {
  const channels = normalizeEnabledChannels(enabledChannels);
  const payload = Buffer.allocUnsafe(4);
  payload.writeUInt32LE(channels.length * 8, 0);
  return createInjectionFrame(INJECTION_FRAME.CONFIG, payload);
}

module.exports = {
  INJECTION_FRAME,
  createInjectionConfig,
  createInjectionFrame,
  encodePxlogicCrossChunk,
  normalizeEnabledChannels,
  reverseBits32,
};
