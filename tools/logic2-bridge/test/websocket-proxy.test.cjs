'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const net = require('node:net');
const {
  ClientFrameRelay,
  decodeMaskedPayload,
  encodeMaskedTextFrame,
  removeCompressionOffer,
  startWebSocketProxy,
} = require('../lib/websocket-proxy.cjs');

class FakeSocket extends EventEmitter {
  constructor() {
    super();
    this.destroyed = false;
    this.frames = [];
  }

  write(data) {
    this.frames.push(Buffer.from(data));
    return true;
  }
}

function maskedTextFrame(text) {
  return encodeMaskedTextFrame(text);
}

function maskedFrame(text, { fin, opcode }) {
  const frame = encodeMaskedTextFrame(text);
  frame[0] = (fin ? 0x80 : 0) | opcode;
  return frame;
}

function decodeTextFrame(frame) {
  const lengthCode = frame[1] & 0x7f;
  const maskOffset = lengthCode === 126 ? 4 : lengthCode === 127 ? 10 : 2;
  const payloadLength = lengthCode === 126
    ? frame.readUInt16BE(2)
    : lengthCode === 127
      ? Number(frame.readBigUInt64BE(2))
      : lengthCode;
  return decodeMaskedPayload(frame, maskOffset + 4, payloadLength, maskOffset).toString('utf8');
}

test('removes compression offer so observed control frames remain JSON text', () => {
  const header = 'GET /saleae HTTP/1.1\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n';
  assert.equal(removeCompressionOffer(header), 'GET /saleae HTTP/1.1\r\n\r\n');
});

test('observes a Logic request before forwarding its frame', async () => {
  const upstream = new FakeSocket();
  let observed = false;
  const relay = new ClientFrameRelay(upstream, async text => {
    assert.equal(text, '{"type":"test"}');
    observed = true;
  });
  const frame = maskedTextFrame('{"type":"test"}');
  relay.push(frame.subarray(0, 4));
  relay.push(frame.subarray(4));
  await relay.finish();
  assert.equal(observed, true);
  assert.deepEqual(upstream.frames, [frame]);
});

test('rewrites a Logic JSON request as a valid masked text frame', async () => {
  const upstream = new FakeSocket();
  const replacement = JSON.stringify({ type: 'request', contents: { actions: [] } });
  const relay = new ClientFrameRelay(upstream, async () => replacement);
  relay.push(maskedTextFrame(JSON.stringify({ type: 'request', contents: {
    actions: [{ type: 'Saleae::Graph::GraphActions::RemoveNode', id: 10010 }],
  } })));
  await relay.finish();
  assert.equal(upstream.frames.length, 1);
  assert.equal((upstream.frames[0][1] & 0x80) !== 0, true);
  assert.equal(decodeTextFrame(upstream.frames[0]), replacement);
});

test('reassembles fragmented text before applying a request transform', async () => {
  const upstream = new FakeSocket();
  const relay = new ClientFrameRelay(upstream, async text => text.replace('RemoveNode', 'Deferred'));
  relay.push(maskedFrame('{"type":"Remove', { fin: false, opcode: 1 }));
  relay.push(maskedFrame('Node"}', { fin: true, opcode: 0 }));
  await relay.finish();
  assert.equal(upstream.frames.length, 1);
  assert.equal(decodeTextFrame(upstream.frames[0]), '{"type":"Deferred"}');
});

test('encodes transformed text larger than 64 KiB with a 64-bit payload length', async () => {
  const upstream = new FakeSocket();
  const replacement = 'x'.repeat(70_000);
  const relay = new ClientFrameRelay(upstream, async () => replacement);
  relay.push(maskedTextFrame('{"type":"request"}'));
  await relay.finish();
  assert.equal(upstream.frames[0][1] & 0x7f, 127);
  assert.equal(decodeTextFrame(upstream.frames[0]), replacement);
});

test('allocates a public port automatically', async () => {
  const proxy = await startWebSocketProxy({
    port: 0,
    backendPort: 1,
    observeText: async () => {},
  });
  assert.equal(Number.isInteger(proxy.port), true);
  assert.equal(proxy.port > 0, true);
  await proxy.close();
});

test('falls back when the requested public port is occupied', async () => {
  const occupied = net.createServer();
  await new Promise((resolve, reject) => {
    occupied.once('error', reject);
    occupied.listen(0, '127.0.0.1', resolve);
  });
  const address = occupied.address();
  const occupiedPort = typeof address === 'object' && address ? address.port : undefined;
  const proxy = await startWebSocketProxy({
    port: occupiedPort,
    backendPort: 1,
    observeText: async () => {},
  });
  assert.notEqual(proxy.port, occupiedPort);
  await proxy.close();
  await new Promise((resolve, reject) => {
    occupied.close(error => error ? reject(error) : resolve());
  });
});
