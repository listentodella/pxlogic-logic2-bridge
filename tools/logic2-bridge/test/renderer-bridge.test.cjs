'use strict';

const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const http = require('node:http');
const test = require('node:test');

const {
  connectRenderer,
  decodeServerFrames,
  encodeClientFrame,
  selectRendererTarget,
} = require('../lib/renderer-bridge.cjs');

const WEBSOCKET_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

/** Unmasks one client frame, which is the only direction the fake has to read. */
function decodeClientFrame(buffer) {
  const length = buffer[1] & 0x7f;
  let offset = 2;
  let payloadLength = length;
  if (length === 126) {
    payloadLength = buffer.readUInt16BE(2);
    offset = 4;
  } else if (length === 127) {
    payloadLength = Number(buffer.readBigUInt64BE(2));
    offset = 10;
  }
  const mask = buffer.subarray(offset, offset + 4);
  const payload = buffer.subarray(offset + 4, offset + 4 + payloadLength);
  const decoded = Buffer.allocUnsafe(payload.length);
  for (let index = 0; index < payload.length; index += 1) {
    decoded[index] = payload[index] ^ mask[index % 4];
  }
  return decoded.toString('utf8');
}

function encodeServerFrame(text, { split = false } = {}) {
  const payload = Buffer.from(text, 'utf8');
  let header;
  if (payload.length < 126) {
    header = Buffer.from([0x81, payload.length]);
  } else if (payload.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  const frame = Buffer.concat([header, payload]);
  if (!split) return [frame];
  const cut = Math.min(header.length + 3, frame.length - 1);
  return [frame.subarray(0, cut), frame.subarray(cut)];
}

/**
 * A stand-in for Chromium's debugging port: the target list over HTTP and one
 * WebSocket that answers `Runtime.evaluate`.
 */
async function startFakeCdp(handler, { targets, splitFrames = false } = {}) {
  const server = http.createServer((request, response) => {
    if (request.url === '/json/list') {
      const port = server.address().port;
      const body = JSON.stringify(
        targets ?? [
          {
            type: 'page',
            url: 'file:///app/index.html?logic_websocket=127.0.0.1:1234',
            webSocketDebuggerUrl: `ws://127.0.0.1:${port}/devtools/page/ABC`,
          },
        ],
      );
      response.writeHead(200, { 'Content-Type': 'application/json' });
      response.end(body);
      return;
    }
    response.writeHead(404).end();
  });

  server.on('upgrade', (request, socket) => {
    const key = request.headers['sec-websocket-key'];
    const accept = crypto
      .createHash('sha1')
      .update(key + WEBSOCKET_GUID)
      .digest('base64');
    socket.write(
      'HTTP/1.1 101 Switching Protocols\r\n' +
        'Upgrade: websocket\r\n' +
        'Connection: Upgrade\r\n' +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    socket.on('data', chunk => {
      let message;
      try {
        message = JSON.parse(decodeClientFrame(chunk));
      } catch {
        return;
      }
      const reply = handler(message);
      if (reply === undefined) return;
      for (const frame of encodeServerFrame(JSON.stringify(reply), { split: splitFrames })) {
        socket.write(frame);
      }
    });
    socket.on('error', () => {});
  });

  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { server, port: server.address().port };
}

test('a masked client frame round-trips through the server decoder', () => {
  const text = 'x'.repeat(200);
  assert.equal(decodeClientFrame(encodeClientFrame(text)), text);
  const short = '{"id":1}';
  assert.equal(decodeClientFrame(encodeClientFrame(short)), short);
});

test('the frame decoder keeps a partial frame instead of guessing', () => {
  const [head, tail] = encodeServerFrame('{"id":7}', { split: true });
  const first = decodeServerFrames(head);
  assert.deepEqual(first.messages, []);
  assert.equal(first.rest.length, head.length);
  const second = decodeServerFrames(Buffer.concat([first.rest, tail]));
  assert.deepEqual(
    second.messages.map(message => message.text),
    ['{"id":7}'],
  );
  assert.equal(second.rest.length, 0);
});

test('the renderer target is chosen by its graph client parameter', () => {
  const chosen = selectRendererTarget([
    { type: 'page', url: 'file:///other.html', webSocketDebuggerUrl: 'ws://a' },
    {
      type: 'page',
      url: 'file:///app.html?logic_websocket=127.0.0.1:1',
      webSocketDebuggerUrl: 'ws://b',
    },
  ]);
  assert.equal(chosen.webSocketDebuggerUrl, 'ws://b');
});

test('a target without a debugger url is never chosen', () => {
  assert.equal(selectRendererTarget([{ type: 'page', url: 'file:///a' }]), null);
  assert.equal(selectRendererTarget([]), null);
});

test('evaluating in the renderer returns the value by value', async t => {
  const { server, port } = await startFakeCdp(message => ({
    id: message.id,
    result: { result: { type: 'object', value: { id: 4, timeSec: 1.25 } } },
  }));
  t.after(() => server.close());
  const session = await connectRenderer(port, { timeoutMs: 2000 });
  t.after(() => session.close());
  assert.deepEqual(await session.evaluate('1'), { id: 4, timeSec: 1.25 });
});

test('a reply split across reads is still reassembled', async t => {
  const { server, port } = await startFakeCdp(
    message => ({
      id: message.id,
      result: { result: { value: { note: 'n'.repeat(300) } } },
    }),
    { splitFrames: true },
  );
  t.after(() => server.close());
  const session = await connectRenderer(port, { timeoutMs: 2000 });
  t.after(() => session.close());
  const value = await session.evaluate('1');
  assert.equal(value.note.length, 300);
});

test('an exception inside the page is reported, not returned as empty', async t => {
  const { server, port } = await startFakeCdp(message => ({
    id: message.id,
    result: {
      exceptionDetails: {
        exception: { description: 'Error: Logic 2 has no active capture session\n    at x' },
      },
    },
  }));
  t.after(() => server.close());
  const session = await connectRenderer(port, { timeoutMs: 2000 });
  t.after(() => session.close());
  await assert.rejects(() => session.evaluate('1'), /no active capture session/);
});

test('a protocol error is reported with its own message', async t => {
  const { server, port } = await startFakeCdp(message => ({
    id: message.id,
    error: { code: -32000, message: 'Cannot find context' },
  }));
  t.after(() => server.close());
  const session = await connectRenderer(port, { timeoutMs: 2000 });
  t.after(() => session.close());
  await assert.rejects(() => session.evaluate('1'), /Cannot find context/);
});

test('a renderer that never answers fails on its own deadline', async t => {
  const { server, port } = await startFakeCdp(() => undefined);
  t.after(() => server.close());
  const session = await connectRenderer(port, { timeoutMs: 120 });
  t.after(() => session.close());
  await assert.rejects(() => session.evaluate('1'), /did not answer within 120 ms/);
});

test('a debugging port with no renderer target says so', async t => {
  const { server, port } = await startFakeCdp(() => undefined, {
    targets: [{ type: 'service_worker', url: 'x' }],
  });
  t.after(() => server.close());
  await assert.rejects(() => connectRenderer(port, { timeoutMs: 500 }), /no Logic 2 renderer/);
});

test('a closed debugging port fails without hanging', async () => {
  const server = http.createServer();
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  await assert.rejects(() => connectRenderer(port, { timeoutMs: 500 }));
});
