'use strict';

/**
 * Timing markers and their notes, which Logic 2's MCP does not expose.
 *
 * The store is reached at `rapidDataStore.activeSession.markers`, a `MarkersStore`
 * whose `addMarker({ timeSec, note, color, label })` is the same call the sidebar makes.
 * `rapidDataStore` is not a global, so it is taken from the React context the app
 * already hands to its own components.
 *
 * Every expression is self-contained and returns a plain object, never a store handle:
 * `Runtime.evaluate` with `returnByValue` has to serialise what comes back, and a MobX
 * observable is full of cycles.
 */

/// Logic 2 accepts these for a marker; anything else is left to its default.
const MARKER_COLORS = new Set([
  'blue',
  'green',
  'orange',
  'pink',
  'purple',
  'red',
  'teal',
  'yellow',
]);
/// A note longer than this is a mistake rather than an annotation.
const MAX_NOTE_LENGTH = 2000;
const MAX_LABEL_LENGTH = 200;

/**
 * Finds `rapidDataStore` from inside the page.
 *
 * The app does not publish it, so this walks React's fibers from the root container to
 * the context value it passes down. That is an internal shape and stated as such: a
 * Logic 2 version that changes it must produce this error rather than a silent miss.
 *
 * Logic 2's bundle contains a `window.__saleaeTest` hook, which would have been the
 * steadier way in. It is not installed in the shipped 2.4.46 build -- probed on the
 * running app and found `undefined` -- so the fiber walk is the only route, not a
 * fallback. It does not expose markers anyway.
 */
const LOCATE_STORE = `(() => {
  // Identified by what it holds rather than by name, because the bundle is minified and
  // the class name is not stable across builds. \`deviceListManager\` is the store's own
  // field, checked in Logic 2's own tool handlers.
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

function quote(value) {
  // JSON is a JavaScript expression, and it escapes what has to be escaped. The line
  // terminators JSON leaves bare are not legal in a JS string literal.
  return JSON.stringify(String(value))
    .replace(/\u2028/g, '\\u2028')
    .replace(/\u2029/g, '\\u2029');
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
  return { timeSec, note, label, color };
}

/**
 * Builds the expression that adds one marker.
 *
 * `label` is assigned after construction: it is an observable on the manager rather
 * than a constructor argument, and Logic 2 fills in a default when it is left alone.
 */
function addMarkerExpression({ timeSec, note, label, color }) {
  const props = [`timeSec: ${timeSec}`];
  if (note !== undefined) props.push(`note: ${quote(note)}`);
  if (color !== undefined) props.push(`color: ${quote(color)}`);
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const marker = session.markers.addMarker({ ${props.join(', ')} });
  ${label !== undefined ? `marker.label = ${quote(label)};` : ''}
  return {
    id: marker.id,
    timeSec: marker.timeSec,
    label: marker.label ?? null,
    note: marker.note ?? null,
    color: marker.color ?? null,
  };
})()`;
}

/** Builds the expression that lists the markers currently on the capture. */
function listMarkersExpression() {
  return `(() => {
  const session = ${ACTIVE_SESSION};
  return Object.values(session.markers.markers).map(marker => ({
    id: marker.id,
    timeSec: marker.timeSec,
    label: marker.label ?? null,
    note: marker.note ?? null,
    color: marker.color ?? null,
  })).sort((left, right) => left.timeSec - right.timeSec);
})()`;
}

/**
 * Builds the expression that removes one marker by id.
 *
 * `delete()` is the manager's own teardown, so the sidebar and the waveform both drop
 * it. Removing the entry from the map alone would leave the drawn marker behind.
 */
function removeMarkerExpression(id) {
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const marker = session.markers.markers[${Number(id)}];
  if (!marker) {
    throw new Error('no timing marker with id ${Number(id)}');
  }
  if (typeof marker.delete === 'function') marker.delete();
  else delete session.markers.markers[${Number(id)}];
  return { removed: ${Number(id)} };
})()`;
}

/** Builds the expression that sets or clears the note on an existing marker. */
function setMarkerNoteExpression(id, note) {
  const assignment = note === undefined || note === null ? 'undefined' : quote(String(note));
  return `(() => {
  const session = ${ACTIVE_SESSION};
  const marker = session.markers.markers[${Number(id)}];
  if (!marker) {
    throw new Error('no timing marker with id ${Number(id)}');
  }
  marker.note = ${assignment};
  return { id: marker.id, note: marker.note ?? null };
})()`;
}

module.exports = {
  ACTIVE_SESSION,
  LOCATE_STORE,
  MARKER_COLORS,
  MAX_LABEL_LENGTH,
  MAX_NOTE_LENGTH,
  addMarkerExpression,
  listMarkersExpression,
  quote,
  removeMarkerExpression,
  setMarkerNoteExpression,
  validateMarkerRequest,
};
