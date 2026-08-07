'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const net = require('node:net');
const {
  ClientFrameRelay,
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
  const payload = Buffer.from(text);
  const mask = Buffer.from([0x11, 0x22, 0x33, 0x44]);
  const frame = Buffer.alloc(2 + 4 + payload.length);
  frame[0] = 0x81;
  frame[1] = 0x80 | payload.length;
  mask.copy(frame, 2);
  for (let index = 0; index < payload.length; index += 1) {
    frame[6 + index] = payload[index] ^ mask[index % 4];
  }
  return frame;
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
