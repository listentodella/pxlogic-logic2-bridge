'use strict';

const net = require('node:net');

const MAX_HANDSHAKE_BYTES = 64 * 1024;
const MAX_OBSERVED_TEXT_BYTES = 32 * 1024 * 1024;

function writeSocket(socket, data) {
  if (socket.destroyed) return Promise.reject(new Error('socket closed'));
  if (socket.write(data)) return Promise.resolve();
  return new Promise((resolve, reject) => {
    const onDrain = () => {
      cleanup();
      resolve();
    };
    const onError = error => {
      cleanup();
      reject(error);
    };
    const cleanup = () => {
      socket.removeListener('drain', onDrain);
      socket.removeListener('error', onError);
    };
    socket.once('drain', onDrain);
    socket.once('error', onError);
  });
}

function decodeMaskedPayload(frame, payloadOffset, payloadLength, maskOffset) {
  const output = Buffer.allocUnsafe(payloadLength);
  for (let index = 0; index < payloadLength; index += 1) {
    output[index] = frame[payloadOffset + index] ^ frame[maskOffset + (index % 4)];
  }
  return output;
}

class ClientFrameRelay {
  constructor(upstream, observeText) {
    this.upstream = upstream;
    this.observeText = observeText;
    this.buffer = Buffer.alloc(0);
    this.forwarding = Promise.resolve();
    this.fragmentText = null;
    this.fragmentTextBytes = 0;
  }

  push(chunk) {
    this.buffer = this.buffer.length ? Buffer.concat([this.buffer, chunk]) : Buffer.from(chunk);
    while (this.consumeFrame()) {
      // Consume all complete frames currently buffered.
    }
  }

  consumeFrame() {
    if (this.buffer.length < 2) return false;
    const first = this.buffer[0];
    const second = this.buffer[1];
    const fin = (first & 0x80) !== 0;
    const rsv = first & 0x70;
    const opcode = first & 0x0f;
    const masked = (second & 0x80) !== 0;
    let payloadLength = second & 0x7f;
    let offset = 2;

    if (rsv !== 0) throw new Error('compressed WebSocket frames are not supported');
    if (payloadLength === 126) {
      if (this.buffer.length < 4) return false;
      payloadLength = this.buffer.readUInt16BE(2);
      offset = 4;
    } else if (payloadLength === 127) {
      if (this.buffer.length < 10) return false;
      const length64 = this.buffer.readBigUInt64BE(2);
      if (length64 > BigInt(Number.MAX_SAFE_INTEGER)) {
        throw new Error('WebSocket frame is too large');
      }
      payloadLength = Number(length64);
      offset = 10;
    }
    if (!masked) throw new Error('Logic sent an unmasked client WebSocket frame');

    const maskOffset = offset;
    const payloadOffset = maskOffset + 4;
    const frameLength = payloadOffset + payloadLength;
    if (this.buffer.length < frameLength) return false;

    const frame = Buffer.from(this.buffer.subarray(0, frameLength));
    this.buffer = this.buffer.subarray(frameLength);
    const observedText = this.collectTextFrame(
      opcode,
      fin,
      frame,
      payloadOffset,
      payloadLength,
      maskOffset,
    );
    this.forwarding = this.forwarding.then(async () => {
      if (observedText !== undefined) await this.observeText(observedText);
      await writeSocket(this.upstream, frame);
    });
    // finish() retains the rejected promise for orderly shutdown; this handler
    // prevents an early unhandled-rejection report before the socket closes.
    this.forwarding.catch(() => {});
    return true;
  }

  collectTextFrame(opcode, fin, frame, payloadOffset, payloadLength, maskOffset) {
    if (opcode === 1) {
      const payload = decodeMaskedPayload(frame, payloadOffset, payloadLength, maskOffset);
      if (fin) return payload.toString('utf8');
      this.fragmentText = [payload];
      this.fragmentTextBytes = payload.length;
      return undefined;
    }
    if (opcode !== 0 || this.fragmentText === null) return undefined;

    const payload = decodeMaskedPayload(frame, payloadOffset, payloadLength, maskOffset);
    this.fragmentTextBytes += payload.length;
    if (this.fragmentTextBytes <= MAX_OBSERVED_TEXT_BYTES) {
      this.fragmentText.push(payload);
    } else {
      this.fragmentText = [];
    }
    if (!fin) return undefined;

    const complete = this.fragmentTextBytes <= MAX_OBSERVED_TEXT_BYTES
      ? Buffer.concat(this.fragmentText, this.fragmentTextBytes).toString('utf8')
      : undefined;
    this.fragmentText = null;
    this.fragmentTextBytes = 0;
    return complete;
  }

  async finish() {
    if (this.buffer.length !== 0) {
      throw new Error('Logic connection ended with a partial WebSocket frame');
    }
    await this.forwarding;
  }
}

function removeCompressionOffer(header) {
  return header.replace(/^Sec-WebSocket-Extensions:[^\r\n]*\r\n/gim, '');
}

function startWebSocketProxy({ port, backendPort, observeText }) {
  const connections = new Set();
  const server = net.createServer(client => {
    const upstream = net.createConnection({ host: '127.0.0.1', port: backendPort });
    const pair = { client, upstream };
    connections.add(pair);
    let handshake = Buffer.alloc(0);
    let upgraded = false;
    let relay;
    let closing = false;

    const closePair = error => {
      if (closing) return;
      closing = true;
      if (error) console.error(`[logic2-bridge:proxy] ${error.message}`);
      client.destroy();
      upstream.destroy();
      connections.delete(pair);
    };

    client.pause();
    upstream.once('connect', () => client.resume());
    upstream.on('data', data => {
      if (!client.destroyed && !client.write(data)) {
        upstream.pause();
        client.once('drain', () => upstream.resume());
      }
    });
    client.on('data', data => {
      try {
        if (!upgraded) {
          handshake = handshake.length ? Buffer.concat([handshake, data]) : Buffer.from(data);
          if (handshake.length > MAX_HANDSHAKE_BYTES) {
            throw new Error('Logic WebSocket handshake exceeded 64 KiB');
          }
          const boundary = handshake.indexOf('\r\n\r\n');
          if (boundary === -1) return;
          const headerEnd = boundary + 4;
          const header = removeCompressionOffer(handshake.subarray(0, headerEnd).toString('latin1'));
          upstream.write(Buffer.from(header, 'latin1'));
          upgraded = true;
          relay = new ClientFrameRelay(upstream, observeText);
          const remaining = handshake.subarray(headerEnd);
          handshake = Buffer.alloc(0);
          if (remaining.length) relay.push(remaining);
          return;
        }
        relay.push(data);
      } catch (error) {
        closePair(error);
      }
    });

    client.on('end', () => {
      const finishing = relay ? relay.finish() : Promise.resolve();
      finishing.then(() => upstream.end()).catch(closePair);
    });
    upstream.on('end', () => client.end());
    client.on('error', closePair);
    upstream.on('error', closePair);
    client.on('close', () => {
      if (!upstream.destroyed) upstream.destroy();
      connections.delete(pair);
    });
    upstream.on('close', () => {
      if (!client.destroyed) client.destroy();
      connections.delete(pair);
    });
  });

  return new Promise((resolve, reject) => {
    let retriedWithAutomaticPort = false;
    const listen = requestedPort => {
      const onError = error => {
        if (error.code === 'EADDRINUSE' && requestedPort !== 0 && !retriedWithAutomaticPort) {
          retriedWithAutomaticPort = true;
          listen(0);
          return;
        }
        reject(error);
      };
      server.once('error', onError);
      server.listen(requestedPort, '127.0.0.1', () => {
        server.removeListener('error', onError);
        const address = server.address();
        const actualPort = typeof address === 'object' && address ? address.port : undefined;
        if (!actualPort) {
          server.close();
          reject(new Error('Could not determine the Graph WebSocket port'));
          return;
        }
        let closePromise;
        resolve({
          server,
          port: actualPort,
          close() {
            if (closePromise) return closePromise;
            closePromise = (async () => {
              for (const { client, upstream } of connections) {
                client.destroy();
                upstream.destroy();
              }
              connections.clear();
              if (!server.listening) return;
              await new Promise((resolveClose, rejectClose) => {
                server.close(error => error ? rejectClose(error) : resolveClose());
              });
            })();
            return closePromise;
          },
        });
      });
    };
    listen(port);
  });
}

module.exports = {
  ClientFrameRelay,
  decodeMaskedPayload,
  removeCompressionOffer,
  startWebSocketProxy,
};
