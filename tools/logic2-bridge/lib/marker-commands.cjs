'use strict';

/**
 * Serves timing-marker commands by talking to Logic 2's renderer.
 *
 * These exist because Logic 2's MCP surface has no marker tool: its fifteen tools are
 * defined inside the renderer and reach `rapidDataStore` directly, and markers live on
 * that same store without a tool in front of them.
 *
 * The connection is made on first use and kept. Connecting per command would pay a
 * handshake every time and, worse, would hide a Logic 2 that had gone away behind what
 * looks like a slow marker.
 */

const { connectRenderer } = require('./renderer-bridge.cjs');
const {
  addMarkerExpression,
  addMarkerPairExpression,
  listMarkersExpression,
  removeMarkerExpression,
  setMarkerNoteExpression,
  validateMarkerRequest,
  validatePairRequest,
} = require('./renderer-markers.cjs');

const MARKER_COMMAND_TYPES = new Set([
  'add-timing-marker',
  'add-timing-marker-pair',
  'list-timing-markers',
  'remove-timing-marker',
  'set-timing-marker-note',
]);

class MarkerCommandService {
  /**
   * @param {object} options
   * @param {number|undefined} options.debuggingPort Port Logic 2 was launched with.
   * @param {(event: object) => void} options.emit Reports the outcome upward.
   * @param {(port: number, options?: object) => Promise<object>} [options.connect]
   */
  constructor({ debuggingPort, emit, connect = connectRenderer }) {
    this.debuggingPort = debuggingPort;
    this.emit = emit;
    this.connect = connect;
    this.session = null;
    this.connecting = null;
  }

  /**
   * Returns a live session, reconnecting if the last one died.
   *
   * Logic 2 restarting closes the socket. That is recoverable, so a dead session is
   * dropped and remade rather than reported as a marker failure.
   */
  async _session() {
    if (this.session && !this.session.closed) return this.session;
    this.session = null;
    if (!this.connecting) {
      this.connecting = this.connect(this.debuggingPort)
        .then(session => {
          this.session = session;
          return session;
        })
        .finally(() => {
          this.connecting = null;
        });
    }
    return this.connecting;
  }

  /** Runs one command and returns the payload to report. */
  async _run(command) {
    switch (command.type) {
      case 'add-timing-marker': {
        const validated = validateMarkerRequest(command);
        if (validated.error) return { ok: false, error: validated.error };
        const session = await this._session();
        return { ok: true, marker: await session.evaluate(addMarkerExpression(validated)) };
      }
      case 'add-timing-marker-pair': {
        const validated = validatePairRequest(command);
        if (validated.error) return { ok: false, error: validated.error };
        const session = await this._session();
        return { ok: true, pair: await session.evaluate(addMarkerPairExpression(validated)) };
      }
      case 'list-timing-markers': {
        const session = await this._session();
        // The renderer answers with both maps; they are reported as it sent them so a
        // capture holding only pairs does not read as an empty one.
        const listed = await session.evaluate(listMarkersExpression());
        return { ok: true, markers: listed?.markers ?? [], pairs: listed?.pairs ?? [] };
      }
      case 'remove-timing-marker': {
        const id = Number(command.id);
        if (!Number.isInteger(id)) return { ok: false, error: 'id must be an integer marker id' };
        const session = await this._session();
        return { ok: true, ...(await session.evaluate(removeMarkerExpression(id))) };
      }
      case 'set-timing-marker-note': {
        const id = Number(command.id);
        if (!Number.isInteger(id)) return { ok: false, error: 'id must be an integer marker id' };
        const note = command.note === undefined || command.note === null ? undefined : String(command.note);
        const session = await this._session();
        return { ok: true, ...(await session.evaluate(setMarkerNoteExpression(id, note))) };
      }
      default:
        return { ok: false, error: `unknown marker command: ${String(command.type)}` };
    }
  }

  /**
   * Handles one command and always reports exactly one result.
   *
   * A command that produced nothing would leave the caller waiting on its own timeout
   * with nothing to show the agent, so every path -- including a missing debugging port
   * and an unexpected throw -- ends in a `timing-marker-result` carrying the request id.
   */
  async handle(command) {
    const requestId = command?.requestId ?? null;
    if (this.debuggingPort === undefined) {
      this.emit({
        type: 'timing-marker-result',
        requestId,
        ok: false,
        error:
          'the renderer channel is off for this session; restart the Bridge to enable timing markers',
      });
      return;
    }
    try {
      const outcome = await this._run(command);
      this.emit({ type: 'timing-marker-result', requestId, ...outcome });
    } catch (error) {
      // A failed evaluation may mean the socket is gone; drop it so the next command
      // reconnects instead of reusing a corpse.
      if (this.session?.closed) this.session = null;
      this.emit({
        type: 'timing-marker-result',
        requestId,
        ok: false,
        error: error?.message ? String(error.message) : String(error),
      });
    }
  }

  close() {
    this.session?.close();
    this.session = null;
  }
}

function isMarkerCommand(type) {
  return MARKER_COMMAND_TYPES.has(String(type));
}

module.exports = { MARKER_COMMAND_TYPES, MarkerCommandService, isMarkerCommand };
