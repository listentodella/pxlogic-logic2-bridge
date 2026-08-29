'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');

const { MarkerCommandService, isMarkerCommand } = require('../lib/marker-commands.cjs');
const {
  addMarkerExpression,
  validateMarkerRequest,
} = require('../lib/renderer-markers.cjs');

/** A renderer stand-in that records the expressions it was asked to evaluate. */
function fakeSession(reply = () => ({ id: 1, timeSec: 0 })) {
  return {
    closed: false,
    evaluated: [],
    async evaluate(expression) {
      this.evaluated.push(expression);
      return reply(expression);
    },
    close() {
      this.closed = true;
    },
  };
}

function serviceWith(session, options = {}) {
  // Checked by presence, not by default: an explicit `undefined` port is the case
  // under test, and a default parameter would quietly replace it with a real one.
  const debuggingPort = 'debuggingPort' in options ? options.debuggingPort : 9227;
  const events = [];
  let connects = 0;
  const service = new MarkerCommandService({
    debuggingPort,
    emit: event => events.push(event),
    connect: async () => {
      connects += 1;
      if (session instanceof Error) throw session;
      return session;
    },
  });
  return { service, events, connects: () => connects };
}

test('only marker command types are claimed by the marker route', () => {
  assert.ok(isMarkerCommand('add-timing-marker'));
  assert.ok(isMarkerCommand('list-timing-markers'));
  assert.ok(isMarkerCommand('remove-timing-marker'));
  assert.ok(isMarkerCommand('set-timing-marker-note'));
  assert.ok(!isMarkerCommand('set-hardware-threshold'));
  assert.ok(!isMarkerCommand(undefined));
});

test('adding a marker reports the marker the renderer created', async () => {
  const session = fakeSession(() => ({ id: 4, timeSec: 1.5, note: 'edge', label: 'A1', color: 'red' }));
  const { service, events } = serviceWith(session);
  await service.handle({ type: 'add-timing-marker', requestId: 'r1', timeSec: 1.5, note: 'edge' });
  assert.deepEqual(events, [
    {
      type: 'timing-marker-result',
      requestId: 'r1',
      ok: true,
      marker: { id: 4, timeSec: 1.5, note: 'edge', label: 'A1', color: 'red' },
    },
  ]);
  assert.match(session.evaluated[0], /addMarker\(\{ timeSec: 1\.5, note: "edge" \}\)/);
});

test('a rejected argument never reaches the renderer', async () => {
  const session = fakeSession();
  const { service, events, connects } = serviceWith(session);
  await service.handle({ type: 'add-timing-marker', requestId: 'r2', timeSec: 'soon' });
  assert.equal(events[0].ok, false);
  assert.match(events[0].error, /finite number of seconds/);
  assert.equal(session.evaluated.length, 0);
  assert.equal(connects(), 0);
});

test('a session with no debugging port is answered rather than left pending', async () => {
  const { service, events, connects } = serviceWith(fakeSession(), { debuggingPort: undefined });
  await service.handle({ type: 'add-timing-marker', requestId: 'r3', timeSec: 1 });
  assert.equal(events.length, 1);
  assert.equal(events[0].ok, false);
  assert.match(events[0].error, /restart the Bridge/);
  assert.equal(connects(), 0);
});

test('a renderer that refuses to connect is reported with its reason', async () => {
  const { service, events } = serviceWith(new Error('CDP handshake rejected: HTTP/1.1 500'));
  await service.handle({ type: 'list-timing-markers', requestId: 'r4' });
  assert.equal(events[0].ok, false);
  assert.match(events[0].error, /handshake rejected/);
});

test('an exception from the page becomes the reported error', async () => {
  const session = fakeSession(() => {
    throw new Error('renderer threw: Error: Logic 2 has no active capture session');
  });
  const { service, events } = serviceWith(session);
  await service.handle({ type: 'list-timing-markers', requestId: 'r5' });
  assert.equal(events[0].ok, false);
  assert.match(events[0].error, /no active capture session/);
});

test('the connection is made once and reused across commands', async () => {
  const session = fakeSession(() => []);
  const { service, connects } = serviceWith(session);
  await service.handle({ type: 'list-timing-markers', requestId: 'a' });
  await service.handle({ type: 'list-timing-markers', requestId: 'b' });
  await service.handle({ type: 'list-timing-markers', requestId: 'c' });
  assert.equal(connects(), 1);
  assert.equal(session.evaluated.length, 3);
});

test('a closed session is replaced instead of reused', async () => {
  const first = fakeSession(() => []);
  const second = fakeSession(() => []);
  const events = [];
  let handed = 0;
  const service = new MarkerCommandService({
    debuggingPort: 9227,
    emit: event => events.push(event),
    connect: async () => {
      handed += 1;
      return handed === 1 ? first : second;
    },
  });
  await service.handle({ type: 'list-timing-markers', requestId: 'a' });
  first.closed = true;
  await service.handle({ type: 'list-timing-markers', requestId: 'b' });
  assert.equal(handed, 2);
  assert.equal(second.evaluated.length, 1);
  assert.ok(events.every(event => event.ok));
});

test('every command reports exactly one result carrying its request id', async () => {
  const session = fakeSession(() => ({ id: 1, note: null }));
  const { service, events } = serviceWith(session);
  for (const command of [
    { type: 'add-timing-marker', requestId: 'p1', timeSec: 0 },
    { type: 'list-timing-markers', requestId: 'p2' },
    { type: 'remove-timing-marker', requestId: 'p3', id: 1 },
    { type: 'set-timing-marker-note', requestId: 'p4', id: 1, note: 'x' },
    { type: 'remove-timing-marker', requestId: 'p5', id: 'not-an-id' },
  ]) {
    await service.handle(command);
  }
  assert.deepEqual(
    events.map(event => event.requestId),
    ['p1', 'p2', 'p3', 'p4', 'p5'],
  );
  assert.ok(events.every(event => event.type === 'timing-marker-result'));
  assert.equal(events[4].ok, false);
});

test('a note is escaped so it cannot end the expression it sits in', () => {
  // Proof by execution rather than by inspecting the text: the expression is run with
  // a stub store, and the note has to arrive as data. A break-out would either fail to
  // parse or run the injected call.
  const validated = validateMarkerRequest({ timeSec: 1, note: '"); alert(1); //' });
  const expression = addMarkerExpression(validated);
  let injected = false;
  let received;
  const sandbox = new Function(
    'window',
    'document',
    'alert',
    `const rapidDataStore = window.rapidDataStore; return ${expression.replace(
      /\(\(\) => \{\n  const session = /,
      '(() => {\n  const session = window.__session ?? ',
    )}`,
  );
  const marker = { id: 9, timeSec: 1, label: null, note: undefined, color: null };
  const fakeWindow = {
    __session: {
      markers: {
        addMarker: props => {
          received = props.note;
          marker.note = props.note;
          return marker;
        },
      },
    },
  };
  const result = sandbox(fakeWindow, undefined, () => {
    injected = true;
  });
  assert.equal(injected, false, 'the injected call must not run');
  assert.equal(received, '"); alert(1); //', 'the note must arrive intact as data');
  assert.equal(result.id, 9);
});
