'use strict';

/**
 * Reaches Logic 2's renderer over the Chrome DevTools Protocol.
 *
 * Logic 2's own MCP tools are defined inside the renderer and read `rapidDataStore`
 * directly, so anything the tools do not expose is still sitting on that store --
 * timing markers among them. There is no protocol for it, only the process, so this
 * connects to the process.
 *
 * Nothing here opens or implies a DevTools window. The debugging port is a transport;
 * the inspector UI is a separate client that we deliberately never act as. In
 * particular `Page.inspect` is never sent, `--auto-open-devtools-for-tabs` is never
 * passed, and no `--enable-automation` banner is involved.
 *
 * The store path is Logic 2's internal shape rather than a published API, so every
 * failure is reported as itself. A version that moved the path has to say so, not
 * look like an empty result.
 */

const crypto = require('node:crypto');
const http = require('node:http');
const net = require('node:net');

/// Chromium answers the target list on the same port as plain HTTP.
const TARGET_LIST_PATH = '/json/list';
const WEBSOCKET_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';
/// A renderer evaluation is a local call; a wait longer than this is a fault, not slowness.
const DEFAULT_TIMEOUT_MS = 5000;
const MAX_HANDSHAKE_BYTES = 64 * 1024;
const MAX_MESSAGE_BYTES = 8 * 1024 * 1024;

function requestJson(port, path, timeoutMs) {
  return new Promise((resolve, reject) => {
    const request = http.get(
      { host: '127.0.0.1', port, path, timeout: timeoutMs },
      response => {
        const chunks = [];
        response.on('data', chunk => chunks.push(chunk));
        response.on('end', () => {
          if (response.statusCode !== 200) {
            reject(new Error(`CDP ${path} returned HTTP ${response.statusCode}`));
            return;
          }
          try {
            resolve(JSON.parse(Buffer.concat(chunks).toString('utf8')));
          } catch (error) {
            reject(new Error(`CDP ${path} returned invalid JSON: ${error.message}`));
          }
        });
      },
    );
    request.on('timeout', () => {
      request.destroy(new Error(`CDP ${path} timed out after ${timeoutMs} ms`));
    });
    request.on('error', reject);
  });
}

/**
 * Picks the target holding Logic 2's UI.
 *
 * Logic 2 runs more than one renderer, so the first page is not reliably the
 * application window. The waveform window is the one that carries the graph client,
 * which every Logic 2 window URL identifies through its `logic_websocket` parameter.
 */
function selectRendererTarget(targets) {
  const pages = targets.filter(target => target.type === 'page' && target.webSocketDebuggerUrl);
  if (!pages.length) return null;
  return (
    pages.find(target => String(target.url).includes('logic_websocket')) ??
    pages.find(target => String(target.url).startsWith('file://')) ??
    pages[0]
  );
}

function encodeClientFrame(text) {
  const payload = Buffer.from(text, 'utf8');
  const mask = crypto.randomBytes(4);
  let header;
  if (payload.length < 126) {
    header = Buffer.from([0x81, 0x80 | payload.length]);
  } else if (payload.length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x81;
    header[1] = 0x80 | 126;
    header.writeUInt16BE(payload.length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x81;
    header[1] = 0x80 | 127;
    header.writeBigUInt64BE(BigInt(payload.length), 2);
  }
  const masked = Buffer.allocUnsafe(payload.length);
  for (let index = 0; index < payload.length; index += 1) {
    masked[index] = payload[index] ^ mask[index % 4];
  }
  return Buffer.concat([header, mask, masked]);
}

/**
 * Pulls whole server frames out of a growing buffer.
 *
 * Server-to-client frames are never masked. Returns the frames it could complete and
 * the bytes that are still a partial frame, because a CDP response larger than the
 * socket's read size arrives in pieces.
 */
function decodeServerFrames(buffer) {
  const messages = [];
  let offset = 0;
  for (;;) {
    if (buffer.length - offset < 2) break;
    const first = buffer[offset];
    const second = buffer[offset + 1];
    const opcode = first & 0x0f;
    let length = second & 0x7f;
    let headerLength = 2;
    if (length === 126) {
      if (buffer.length - offset < 4) break;
      length = buffer.readUInt16BE(offset + 2);
      headerLength = 4;
    } else if (length === 127) {
      if (buffer.length - offset < 10) break;
      const big = buffer.readBigUInt64BE(offset + 2);
      if (big > BigInt(MAX_MESSAGE_BYTES)) {
        throw new Error('CDP frame exceeded the message limit');
      }
      length = Number(big);
      headerLength = 10;
    }
    if (length > MAX_MESSAGE_BYTES) {
      throw new Error('CDP frame exceeded the message limit');
    }
    if (buffer.length - offset < headerLength + length) break;
    const payload = buffer.subarray(offset + headerLength, offset + headerLength + length);
    offset += headerLength + length;
    // 0x1 text, 0x2 binary, 0x8 close. Control frames other than close are ignored:
    // Chromium does not ping this connection, and a pong needs no reply.
    if (opcode === 0x1) messages.push({ type: 'text', text: payload.toString('utf8') });
    else if (opcode === 0x8) messages.push({ type: 'close' });
  }
  return { messages, rest: buffer.subarray(offset) };
}

/**
 * One CDP connection to the renderer.
 *
 * Deliberately minimal: a handshake, `Runtime.evaluate`, and a close. Domains are not
 * enabled and no events are subscribed to, so the renderer does nothing extra for
 * being connected to.
 */
class RendererSession {
  constructor(socket, timeoutMs) {
    this.socket = socket;
    this.timeoutMs = timeoutMs;
    this.nextId = 0;
    this.pending = new Map();
    this.buffer = Buffer.alloc(0);
    this.closed = false;
    socket.on('data', chunk => this._consume(chunk));
    socket.on('error', error => this._failAll(error));
    socket.on('close', () => this._failAll(new Error('CDP connection closed')));
  }

  _consume(chunk) {
    this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : Buffer.from(chunk);
    let decoded;
    try {
      decoded = decodeServerFrames(this.buffer);
    } catch (error) {
      this._failAll(error);
      this.socket.destroy();
      return;
    }
    this.buffer = decoded.rest;
    for (const message of decoded.messages) {
      if (message.type === 'close') {
        this._failAll(new Error('CDP connection closed by Logic 2'));
        this.socket.destroy();
        return;
      }
      let parsed;
      try {
        parsed = JSON.parse(message.text);
      } catch {
        continue;
      }
      const settle = this.pending.get(parsed.id);
      if (!settle) continue;
      this.pending.delete(parsed.id);
      settle.resolve(parsed);
    }
  }

  _failAll(error) {
    this.closed = true;
    for (const settle of this.pending.values()) settle.reject(error);
    this.pending.clear();
  }

  send(method, params) {
    if (this.closed) return Promise.reject(new Error('CDP connection is closed'));
    const id = (this.nextId += 1);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} did not answer within ${this.timeoutMs} ms`));
      }, this.timeoutMs);
      this.pending.set(id, {
        resolve: value => {
          clearTimeout(timer);
          resolve(value);
        },
        reject: error => {
          clearTimeout(timer);
          reject(error);
        },
      });
      this.socket.write(encodeClientFrame(JSON.stringify({ id, method, params })));
    });
  }

  /**
   * Evaluates an expression in the renderer and returns its value.
   *
   * `awaitPromise` is on because the store calls this drives are synchronous but the
   * wrappers around them are not, and a pending promise is not an answer. Both failure
   * shapes are surfaced: a protocol error, and a thrown exception inside the page.
   */
  async evaluate(expression) {
    const response = await this.send('Runtime.evaluate', {
      expression,
      returnByValue: true,
      awaitPromise: true,
    });
    if (response.error) {
      throw new Error(`CDP evaluate failed: ${response.error.message}`);
    }
    const details = response.result?.exceptionDetails;
    if (details) {
      const thrown =
        details.exception?.description ?? details.exception?.value ?? details.text ?? 'unknown';
      throw new Error(`renderer threw: ${String(thrown).split('\n')[0]}`);
    }
    return response.result?.result?.value;
  }

  close() {
    this.closed = true;
    this.socket.destroy();
  }
}

function openWebSocket(debuggerUrl, timeoutMs) {
  const url = new URL(debuggerUrl);
  const key = crypto.randomBytes(16).toString('base64');
  const expected = crypto
    .createHash('sha1')
    .update(key + WEBSOCKET_GUID)
    .digest('base64');
  return new Promise((resolve, reject) => {
    const socket = net.connect(
      { host: url.hostname, port: Number(url.port), family: 4 },
      () => {
        socket.write(
          `GET ${url.pathname}${url.search} HTTP/1.1\r\n` +
            `Host: ${url.host}\r\n` +
            'Upgrade: websocket\r\n' +
            'Connection: Upgrade\r\n' +
            `Sec-WebSocket-Key: ${key}\r\n` +
            'Sec-WebSocket-Version: 13\r\n\r\n',
        );
      },
    );
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`CDP handshake timed out after ${timeoutMs} ms`));
    }, timeoutMs);
    let handshake = Buffer.alloc(0);
    const onData = chunk => {
      handshake = Buffer.concat([handshake, chunk]);
      if (handshake.length > MAX_HANDSHAKE_BYTES) {
        clearTimeout(timer);
        socket.destroy();
        reject(new Error('CDP handshake exceeded 64 KiB'));
        return;
      }
      const boundary = handshake.indexOf('\r\n\r\n');
      if (boundary < 0) return;
      clearTimeout(timer);
      socket.removeListener('data', onData);
      const header = handshake.subarray(0, boundary).toString('latin1');
      if (!/^HTTP\/1\.1 101/i.test(header)) {
        socket.destroy();
        reject(new Error(`CDP handshake rejected: ${header.split('\r\n')[0]}`));
        return;
      }
      const accept = /sec-websocket-accept:\s*(\S+)/i.exec(header);
      if (!accept || accept[1] !== expected) {
        socket.destroy();
        reject(new Error('CDP handshake returned a bad accept key'));
        return;
      }
      const session = new RendererSession(socket, timeoutMs);
      // Bytes after the header belong to the first frames.
      const rest = handshake.subarray(boundary + 4);
      if (rest.length) session._consume(rest);
      resolve(session);
    };
    socket.on('data', onData);
    socket.once('error', error => {
      clearTimeout(timer);
      reject(error);
    });
  });
}

/** Connects to the renderer, or explains which step failed. */
async function connectRenderer(port, { timeoutMs = DEFAULT_TIMEOUT_MS } = {}) {
  const targets = await requestJson(port, TARGET_LIST_PATH, timeoutMs);
  if (!Array.isArray(targets)) {
    throw new Error('CDP target list was not an array');
  }
  const target = selectRendererTarget(targets);
  if (!target) {
    throw new Error('no Logic 2 renderer target is exposed on the debugging port');
  }
  return openWebSocket(target.webSocketDebuggerUrl, timeoutMs);
}

module.exports = {
  DEFAULT_TIMEOUT_MS,
  RendererSession,
  connectRenderer,
  decodeServerFrames,
  encodeClientFrame,
  selectRendererTarget,
};
