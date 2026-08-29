'use strict';

const assert = require('node:assert/strict');
const test = require('node:test');

const { MarkerCommandService, isMarkerCommand } = require('../lib/marker-commands.cjs');
const {
  MARKER_COLORS,
  addMarkerExpression,
  addMarkerPairExpression,
  removeMarkerExpression,
  setMarkerNoteExpression,
  validateMarkerRequest,
  validatePairRequest,
} = require('../lib/renderer-markers.cjs');

/**
 * Runs a marker expression against a stand-in page.
 *
 * The session is reached the way the real one is -- `document.getElementById('root')`, the
 * React container key, then the context value on a provider's props -- rather than by
 * substituting the lookup away. That keeps the fiber walk and Logic 2's own
 * `canAddAnnotations` gate under test instead of short-circuiting past both.
 */
function evaluateInFakePage(expression, session) {
  const store = {
    deviceListManager: {},
    activeSessionOptional: session,
  };
  const root = {
    // The spelling verified against Logic 2 2.4.46.
    '__reactContainere$fake': {
      child: {
        memoizedProps: { value: store },
      },
    },
  };
  const document = {
    getElementById: id => (id === 'root' ? root : null),
  };
  return new Function('document', `return ${expression};`)(document);
}

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
  assert.ok(isMarkerCommand('add-timing-marker-pair'));
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
  const marker = { id: 9, timeSec: 1, label: null, note: undefined, color: null };
  // `alert` is a real binding in the sandbox, so a break-out would run rather than
  // throw a reference error that could be mistaken for the escaping having worked.
  globalThis.alert = () => {
    injected = true;
  };
  try {
    const result = evaluateInFakePage(expression, {
      canAddAnnotations: true,
      markers: {
        addMarker: props => {
          received = props.note;
          marker.note = props.note;
          return marker;
        },
      },
    });
    assert.equal(result.id, 9);
  } finally {
    delete globalThis.alert;
  }
  assert.equal(injected, false, 'the injected call must not run');
  assert.equal(received, '"); alert(1); //', 'the note must arrive intact as data');
});

test('only colours Logic 2 can render are accepted', () => {
  // The sidebar renders `darkColors[color]`, so a name outside that map is not an error
  // -- it resolves to undefined and the colour is silently lost. These three were
  // advertised before and did nothing.
  for (const dead of ['blue', 'pink', 'teal']) {
    const rejected = validateMarkerRequest({ timeSec: 1, color: dead });
    assert.match(rejected.error, /color must be one of/, dead);
  }
  // Logic 2's own palette, and the plain names from the same map.
  for (const live of ['paleRed', 'green2', 'purple2', 'orange2', 'fuchsia', 'lightBlue', 'red']) {
    const accepted = validateMarkerRequest({ timeSec: 1, color: live });
    assert.equal(accepted.error, undefined, live);
    assert.equal(accepted.color, live);
  }
  // Every advertised colour is a real key, checked against the set the module exports.
  for (const name of MARKER_COLORS) {
    assert.equal(validateMarkerRequest({ timeSec: 0, color: name }).error, undefined, name);
  }
});

test('an annotation is refused while the capture is still running', () => {
  // Logic 2's own gate: `canAddAnnotations` is `captureFinished` for a non-MSO device, so
  // the app never creates a marker mid-capture. Writing through it anyway would produce
  // markers in a state Logic 2 does not make them in.
  const expression = addMarkerExpression(validateMarkerRequest({ timeSec: 1 }));
  const marker = { id: 0, timeSec: 1, label: '0', note: undefined, color: 'paleRed' };
  const withState = canAddAnnotations => ({
    canAddAnnotations,
    markers: { addMarker: () => marker },
  });

  assert.throws(() => evaluateInFakePage(expression, withState(false)), /finished/);
  assert.equal(evaluateInFakePage(expression, withState(true)).id, 0);
  // A build without the property is annotated as before, not blocked by a check that no
  // longer exists there.
  assert.equal(
    evaluateInFakePage(expression, { markers: { addMarker: () => marker } }).id,
    0,
  );
});

test('the store is found the way the real page exposes it', () => {
  // The walk is the only route in: `window.__saleaeTest` is in the bundle but is not
  // installed in the shipped build. A page that does not lead to a store has to say so
  // rather than report an empty capture.
  const expression = addMarkerExpression(validateMarkerRequest({ timeSec: 0 }));
  const emptyPage = new Function(
    'document',
    `return ${expression};`,
  );
  assert.throws(
    () => emptyPage({ getElementById: () => null }),
    /cannot reach the Logic 2 renderer store/,
  );
  // A root with no React container key is the same failure, not a different one.
  assert.throws(
    () => emptyPage({ getElementById: () => ({}) }),
    /cannot reach the Logic 2 renderer store/,
  );
  // A store whose session is gone is a distinct, actionable message.
  assert.throws(
    () => evaluateInFakePage(expression, undefined),
    /no active capture session/,
  );
});

test('a pair needs two distinct times and reports its duration', () => {
  assert.match(validatePairRequest({ startSec: 1 }).error, /both be finite/);
  assert.match(validatePairRequest({ startSec: 1, endSec: 1 }).error, /must differ/);
  assert.match(validatePairRequest({ startSec: -1, endSec: 2 }).error, /cannot be negative/);
  // Logic 2 keeps a pair's times ordered, so the reply matches the sidebar rather than
  // echoing a backwards request.
  const reversed = validatePairRequest({ startSec: 2, endSec: 0.5 });
  assert.equal(reversed.startSec, 0.5);
  assert.equal(reversed.endSec, 2);
});

test('a pair is built from two markers because that is the only way in', async () => {
  // `PairManager` is module-private; `createPairFromMarkers` consumes two markers and
  // returns the pair. The note goes on the pair, not the markers, because that method
  // joins two notes with a comma and would otherwise duplicate it.
  const created = [];
  const paired = [];
  const pair = { id: 2, label: '2', note: undefined, color: 'red', timesSec: [0.25, 0.75] };
  const store = {
    canAddAnnotations: true,
    markers: {
      addMarker: props => {
        const marker = { ...props, id: created.length, delete: () => created.pop() };
        created.push(marker);
        return marker;
      },
      createPairFromMarkers: markers => {
        paired.push(markers.map(marker => marker.timeSec));
        return pair;
      },
    },
  };
  const expression = addMarkerPairExpression(
    validatePairRequest({ startSec: 0.25, endSec: 0.75, note: 'burst', color: 'red' }),
  );
  const result = evaluateInFakePage(expression, store);

  assert.deepEqual(paired, [[0.25, 0.75]]);
  assert.equal(result.durationSec, 0.5);
  assert.equal(result.startSec, 0.25);
  assert.equal(result.endSec, 0.75);
  assert.equal(pair.note, 'burst', 'the note belongs to the pair');
  assert.equal(created.length, 2, 'both markers were handed to the pair');
});

test('a failed pairing leaves no stray markers behind', () => {
  // Otherwise a retry piles up markers the agent never asked for and cannot see the ids
  // of, because the failure returned no ids.
  const live = new Set();
  const store = {
    canAddAnnotations: true,
    markers: {
      addMarker: props => {
        const marker = { ...props, id: live.size, delete: () => live.delete(marker) };
        live.add(marker);
        return marker;
      },
      createPairFromMarkers: () => {
        throw new Error('Invalid number of markers provided: 2');
      },
    },
  };
  const expression = addMarkerPairExpression(validatePairRequest({ startSec: 0, endSec: 1 }));
  assert.throws(() => evaluateInFakePage(expression, store), /Invalid number of markers/);
  assert.equal(live.size, 0, 'both markers were cleaned up');
});

test('listing reports pairs alongside markers', async () => {
  // Reading only `markers` reported a capture holding a pair as empty, which an agent
  // reads as "nothing is annotated" rather than "look in the other map".
  const session = fakeSession(() => ({
    markers: [{ id: 0, timeSec: 1 }],
    pairs: [{ id: 1, startSec: 2, endSec: 3, durationSec: 1 }],
  }));
  const { service, events } = serviceWith(session);
  await service.handle({ type: 'list-timing-markers', requestId: 'L1' });
  assert.equal(events[0].ok, true);
  assert.equal(events[0].markers.length, 1);
  assert.equal(events[0].pairs.length, 1);
  assert.equal(events[0].pairs[0].durationSec, 1);
  // Both maps are read in one evaluation rather than two round trips.
  assert.equal(session.evaluated.length, 1);
  assert.match(session.evaluated[0], /store\.pairs/);
});

test('a renderer that answers without pairs still reports a usable shape', async () => {
  // An older build, or one that moved `pairs`, must not turn the reply into undefined
  // that the agent then reads as a missing field.
  const session = fakeSession(() => ({ markers: [{ id: 0, timeSec: 1 }] }));
  const { service, events } = serviceWith(session);
  await service.handle({ type: 'list-timing-markers', requestId: 'L2' });
  assert.deepEqual(events[0].pairs, []);
  assert.equal(events[0].markers.length, 1);
});

test('one id sequence spans markers and pairs, so removal accepts either', () => {
  // `getNextId` maxes over both maps, so an id an agent read from a list is unambiguous
  // and asking which kind it meant would be asking for something it cannot know.
  for (const expression of [removeMarkerExpression(3), setMarkerNoteExpression(3, 'x')]) {
    assert.match(expression, /store\.markers \?\? \{\}\)\[3\] \?\? \(store\.pairs/);
    assert.match(expression, /no timing marker or pair with id 3/);
  }
});
