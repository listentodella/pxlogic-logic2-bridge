'use strict';

/**
 * Timing markers and their notes, which Logic 2's MCP does not expose.
 *
 * The store is reached at `rapidDataStore.activeSession.markers`, a `MarkersStore`
 * whose `addMarker({ timeSec, note, color })` is the same call the sidebar makes.
 * `rapidDataStore` is not a global, so it is taken from the React context the app
 * already hands to its own components.
 *
 * Shapes here were read from Logic 2 2.4.46's own sources, which ship as a source map
 * (`dist/logic/bundle.js.map`) inside `app.asar`: `timingMarkers/Store.ts`,
 * `BaseManager.ts`, `MarkerManager.ts`, `PairManager.ts`. Guessing was not necessary and
 * should not be necessary again -- read that map before changing anything below.
 *
 * Every expression is self-contained and returns a plain object, never a store handle:
 * `Runtime.evaluate` with `returnByValue` has to serialise what comes back, and a MobX
 * observable is full of cycles.
 */

/**
 * Colours a marker can take.
 *
 * `MarkerManager.color` is typed `DarkColor = keyof typeof darkColors`, and the sidebar
 * renders it as `darkColors[manager.color || DEFAULT_COLOR]`. A name outside that map
 * does not throw -- it resolves to `undefined` and the colour is silently dropped -- so
 * validating against the real keys is the only way an agent's request means anything.
 *
 * The first six are the palette Logic 2 itself cycles through for new markers
 * (`markerColors` in `shared/utils/colors.ts`). The rest are plain names from the same
 * map, kept because an agent asks for "red" far sooner than for "paleRed". Every name
 * here was checked against that map's 101 keys.
 */
const MARKER_COLORS = new Set([
  'paleRed',
  'green2',
  'purple2',
  'orange2',
  'fuchsia',
  'lightBlue',
  'red',
  'green',
  'orange',
  'purple',
  'yellow',
]);
/// A note longer than this is a mistake rather than an annotation.
const MAX_NOTE_LENGTH = 2000;
const MAX_LABEL_LENGTH = 200;

/**
 * Finds `rapidDataStore` from inside the page.
 *
 * The app does not publish it -- confirmed against its sources, which touch neither
 * `window` nor `globalThis` for this -- so this walks React's fibers from the root
 * container to the context value it passes down. That is an internal shape and stated as
 * such: a Logic 2 version that changes it must produce this error rather than a silent
 * miss.
 *
 * Logic 2's bundle contains a `window.__saleaeTest` hook, which would have been the
 * steadier way in. It is not installed in the shipped 2.4.46 build -- probed on the
 * running app and found `undefined` -- so the fiber walk is the only route, not a
 * fallback. It does not expose markers anyway.
 */
const LOCATE_STORE = `(() => {
  // Identified by what it holds rather than by name, because the bundle is minified and
  // the class name is not stable across builds. \`deviceListManager\` is the store's own
  // field, read the same way by Logic 2's own tool handlers.
  const looksLikeStore = value =>
    !!value &&
    typeof value === 'object' &&
    'deviceListManager' in value &&
    ('activeSessionOptional' in value || 'activeSession' in value);

  const root = document.getElementById('root');
  if (!root) return null;
  // Verified against Logic 2 2.4.46: the key is \`__reactContainere$<hash>\`. The other
  // two forms are React's older and newer spellings, accepted so a bundler or React
  // upgrade does not break this on its own.
  const key = Object.keys(root).find(
    name =>
      name.startsWith('__reactContainere$') ||
      name.startsWith('__reactContainer$') ||
      name.startsWith('__reactFiber$') ||
      name.startsWith('__reactInternalInstance$'),
  );
  if (!key) return null;

  // The store arrives as a React context value, so it is found on a provider's props
  // rather than on a component instance. Reached within ~60 fibers in practice; the
  // ceiling only exists so a shape change cannot spin here.
  const seen = new Set();
  let queue = [root[key]];
  let visited = 0;
  while (queue.length && visited < 200000) {
    const fiber = queue.shift();
    visited += 1;
    if (!fiber || typeof fiber !== 'object' || seen.has(fiber)) continue;
    seen.add(fiber);
    for (const slotName of ['memoizedProps', 'pendingProps', 'memoizedState', 'stateNode']) {
      const slot = fiber[slotName];
      if (looksLikeStore(slot)) return slot;
      if (slot && typeof slot === 'object') {
        if (looksLikeStore(slot.value)) return slot.value;
        for (const nested of Object.values(slot)) {
          if (looksLikeStore(nested)) return nested;
        }
      }
    }
    if (fiber.child) queue.push(fiber.child);
    if (fiber.sibling) queue.push(fiber.sibling);
    if (fiber.alternate) queue.push(fiber.alternate);
  }
  return null;
})()`;

/** Resolves the active session, or says why there is none. */
const ACTIVE_SESSION = `(() => {
  const store = ${LOCATE_STORE};
  if (!store) {
    throw new Error('cannot reach the Logic 2 renderer store; this Logic 2 version may have moved it');
  }
  const session = store.activeSessionOptional ?? store.activeSession;
  if (!session) {
    throw new Error('Logic 2 has no active capture session; start or load a capture first');
  }
  if (!session.markers || typeof session.markers.addMarker !== 'function') {
    throw new Error('the timing marker store is missing; this Logic 2 version may have moved it');
  }
  return session;
})()`;

/**
 * Resolves a session that will actually accept an annotation.
 *
 * `SessionStore.canAddAnnotations` is Logic 2's own gate on timing markers: for a
 * non-MSO device it is `captureFinished`, so the app itself refuses to annotate a
 * capture that is still running. Writing through that check anyway would produce
 * markers under a condition Logic 2 never creates them in, which is not a state worth
 * discovering later, so this refuses on the same terms and says why.
 *
 * Only an explicit \`false\` refuses. A build without the property is annotated as
 * before rather than blocked by a check that no longer exists.
 */
const ANNOTATABLE_SESSION = `(() => {
  const session = ${ACTIVE_SESSION};
  if (session.canAddAnnotations === false) {
    throw new Error('Logic 2 will not annotate this capture yet; markers need a finished (or paused) capture');
  }
  return session;
})()`;

function quote(value) {
  // JSON is a JavaScript expression, and it escapes what has to be escaped. The line
  // terminators JSON leaves bare are not legal in a JS string literal.
  return JSON.stringify(String(value))
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
}

/** Shared validation for the optional fields a marker and a pair both carry. */
function validateAnnotationFields(request) {
  const note = request?.note === undefined || request?.note === null ? undefined : String(request.note);
  if (note !== undefined && note.length > MAX_NOTE_LENGTH) {
    return { error: `note is longer than ${MAX_NOTE_LENGTH} characters` };
  }
  const label =
    request?.label === undefined || request?.label === null ? undefined : String(request.label);
  if (label !== undefined && label.length > MAX_LABEL_LENGTH) {
    return { error: `label is longer than ${MAX_LABEL_LENGTH} characters` };
  }
  const color = request?.color === undefined || request?.color === null ? undefined : String(request.color);
  if (color !== undefined && !MARKER_COLORS.has(color)) {
    return {
      error: `color must be one of: ${Array.from(MARKER_COLORS).sort().join(', ')}`,
    };
  }
  return { note, label, color };
}

/**
 * Rejects a marker request before it reaches the renderer.
 *
 * Returns the cleaned values or the reason, so the caller answers the agent instead of
 * letting a bad argument turn into a page exception.
 */
function validateMarkerRequest(request) {
  const timeSec = Number(request?.timeSec);
  if (!Number.isFinite(timeSec)) {
    return { error: 'timeSec must be a finite number of seconds from the capture start' };
  }
  if (timeSec < 0) {
    return { error: 'timeSec cannot be negative' };
  }
  const fields = validateAnnotationFields(request);
  if (fields.error) return fields;
  return { timeSec, ...fields };
}

/** Rejects a pair request, which needs two distinct times rather than one. */
function validatePairRequest(request) {
  const startSec = Number(request?.startSec);
  const endSec = Number(request?.endSec);
  if (!Number.isFinite(startSec) || !Number.isFinite(endSec)) {
    return { error: 'startSec and endSec must both be finite numbers of seconds from the capture start' };
  }
  if (startSec < 0 || endSec < 0) {
    return { error: 'startSec and endSec cannot be negative' };
  }
  if (startSec === endSec) {
    return { error: 'startSec and endSec must differ; a pair measures an interval' };
  }
  const fields = validateAnnotationFields(request);
  if (fields.error) return fields;
  // Logic 2 keeps a pair's times ordered, so ordering here makes the reply match what
  // the sidebar will show rather than echoing the request back.
  return { startSec: Math.min(startSec, endSec), endSec: Math.max(startSec, endSec), ...fields };
}

/** Fields shared by every marker and pair this module reports. */
const REPORT_COMMON = `
    id: item.id,
    label: item.label ?? null,
    note: item.note ?? null,
    color: item.color ?? null,`;

/**
 * Builds the expression that adds one marker.
 *
 * `label` is assigned after construction: it is an observable on the manager rather
 * than a constructor argument, and Logic 2 defaults it to the marker's own id.
 */
function addMarkerExpression({ timeSec, note, label, color }) {
  const props = [`timeSec: ${timeSec}`];
  if (note !== undefined) props.push(`note: ${quote(note)}`);
  if (color !== undefined) props.push(`color: ${quote(color)}`);
  return `(() => {
  const session = ${ANNOTATABLE_SESSION};
  const item = session.markers.addMarker({ ${props.join(', ')} });
  ${label !== undefined ? `item.label = ${quote(label)};` : ''}
  return {${REPORT_COMMON}
    timeSec: item.timeSec,
  };
})()`;
}

/**
 * Builds the expression that adds a marker pair -- Logic 2's interval measurement.
 *
 * There is no constructor to call from here: `PairManager` is a module-private class, and
 * `createPairFromMarkers` is the only public way in. It takes two existing markers,
 * consumes them, and returns the pair, which is why this adds two markers first. The
 * note is set on the pair afterwards rather than on the markers, because that method
 * joins two notes with a comma and would otherwise duplicate it.
 */
function addMarkerPairExpression({ startSec, endSec, note, label, color }) {
  const colorProp = color !== undefined ? `, color: ${quote(color)}` : '';
  return `(() => {
  const session = ${ANNOTATABLE_SESSION};
  const store = session.markers;
  if (typeof store.createPairFromMarkers !== 'function') {
    throw new Error('this Logic 2 version has no marker pairs');
  }
  const left = store.addMarker({ timeSec: ${startSec}${colorProp} });
  const right = store.addMarker({ timeSec: ${endSec}${colorProp} });
  let item;
  try {
    item = store.createPairFromMarkers([left, right]);
  } catch (error) {
    // The two markers must not outlive a failed pairing, or a retry would pile up
    // strays that the agent never asked for and cannot see the ids of.
    for (const stray of [left, right]) {
      try {
        if (typeof stray.delete === 'function') stray.delete();
      } catch (ignored) {}
    }
    throw error;
  }
  ${note !== undefined ? `item.note = ${quote(note)};` : ''}
  ${label !== undefined ? `item.label = ${quote(label)};` : ''}
  return {${REPORT_COMMON}
    startSec: item.timesSec[0],
    endSec: item.timesSec[1],
    durationSec: item.timesSec[1] - item.timesSec[0],
  };
})()`;
}

/**
 * Builds the expression that lists what is on the capture.
 *
 * Pairs are reported alongside markers because they sit in the same sidebar and share
 * one id sequence: reading only `markers` reported an empty capture as empty while the
 * user was looking at a pair in the list.
 */
function listMarkersExpression() {
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const store = session.markers;
  const describe = item => ({${REPORT_COMMON}
  });
  const markers = Object.values(store.markers ?? {})
    .map(item => ({ ...describe(item), timeSec: item.timeSec }))
    .sort((left, right) => left.timeSec - right.timeSec);
  const pairs = Object.values(store.pairs ?? {})
    .map(item => ({
      ...describe(item),
      startSec: item.timesSec[0],
      endSec: item.timesSec[1],
      durationSec: item.timesSec[1] - item.timesSec[0],
    }))
    .sort((left, right) => left.startSec - right.startSec);
  return { markers, pairs };
})()`;
}

/**
 * Resolves one annotation by id, whether it is a marker or a pair.
 *
 * The two share a single id sequence (`getNextId` spans both maps), so an id an agent
 * read from a list is unambiguous and asking it to say which kind it meant would be
 * asking for something it cannot know.
 */
function locateAnnotation(id) {
  const numeric = Number(id);
  return `(() => {
  const store = session.markers;
  const item = (store.markers ?? {})[${numeric}] ?? (store.pairs ?? {})[${numeric}];
  if (!item) {
    throw new Error('no timing marker or pair with id ${numeric}');
  }
  return item;
})()`;
}

/**
 * Builds the expression that removes one marker or pair by id.
 *
 * `delete()` is the manager's own teardown -- it drops the entry and unregisters from
 * the session's time manager. Removing the map entry alone would leave the drawn marker
 * behind.
 */
function removeMarkerExpression(id) {
  const numeric = Number(id);
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const item = ${locateAnnotation(id)};
  if (typeof item.delete !== 'function') {
    throw new Error('this Logic 2 version does not let a timing marker be removed');
  }
  item.delete();
  return { removed: ${numeric} };
})()`;
}

/** Builds the expression that sets or clears the note on an existing marker or pair. */
function setMarkerNoteExpression(id, note) {
  const assignment = note === undefined || note === null ? 'undefined' : quote(String(note));
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const item = ${locateAnnotation(id)};
  item.note = ${assignment};
  return { id: item.id, note: item.note ?? null };
})()`;
}

module.exports = {
  ACTIVE_SESSION,
  ANNOTATABLE_SESSION,
  LOCATE_STORE,
  MARKER_COLORS,
  MAX_LABEL_LENGTH,
  MAX_NOTE_LENGTH,
  addMarkerExpression,
  addMarkerPairExpression,
  listMarkersExpression,
  quote,
  removeMarkerExpression,
  setMarkerNoteExpression,
  validateMarkerRequest,
  validatePairRequest,
};
