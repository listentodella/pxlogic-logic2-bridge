'use strict';

const GRAPH_ACTION_DATA = 'Saleae::Graph::GraphActionData';
const GRAPH_INIT_REQUEST = 'Saleae::Graph::GraphInitRequestData';
const DELETE_GRAPH_DATA = 'Saleae::Graph::DeleteGraphData';
const ADD_NODE = 'Saleae::Graph::GraphActions::AddNode';
const REMOVE_NODE = 'Saleae::Graph::GraphActions::RemoveNode';
const ROUTE_ACTION = 'Saleae::Graph::GraphActions::RouteAction';
const ANALYZER_NODE = 'Saleae::Graph::AnalyzerNode';
const START_CAPTURE = 'Saleae::Graph::LogicDevice::StartCapture';
const STOP_CAPTURE = 'Saleae::Graph::LogicDevice::StopCapture';

function messageContents(message) {
  let wrapped;
  try {
    wrapped = typeof message === 'string' ? JSON.parse(message) : message;
  } catch {
    return null;
  }
  const contents = wrapped?.type === 'request' ? wrapped.contents : wrapped;
  return contents && typeof contents === 'object' ? { wrapped, contents } : null;
}

function routedActionType(action) {
  return action?.type === ROUTE_ACTION ? action.action?.type : undefined;
}

class GraphActionGuard {
  constructor({ log = message => console.error(message) } = {}) {
    this.log = log;
    this.sessions = new Map();
  }

  session(sessionId) {
    const key = String(sessionId ?? 'default');
    let state = this.sessions.get(key);
    if (!state) {
      state = {
        analyzerNodes: new Set(),
        captureActive: false,
        deferredAnalyzerRemovals: new Set(),
      };
      this.sessions.set(key, state);
    }
    return { key, state };
  }

  transform(message) {
    const parsed = messageContents(message);
    if (!parsed) return undefined;
    const { wrapped, contents } = parsed;

    if (contents.type === GRAPH_INIT_REQUEST) {
      this.sessions.delete(String(contents.newSessionId ?? contents.meta?.sessionId ?? 'default'));
      return undefined;
    }
    if (contents.type === DELETE_GRAPH_DATA) {
      this.sessions.delete(String(contents.meta?.sessionId ?? 'default'));
      return undefined;
    }
    if (contents.type !== GRAPH_ACTION_DATA || !Array.isArray(contents.actions)) {
      return undefined;
    }

    const { key, state } = this.session(contents.meta?.sessionId);
    const actions = [];
    let changed = false;
    let stoppedCapture = false;

    for (const action of contents.actions) {
      if (action?.type === ADD_NODE && action.nodeType === ANALYZER_NODE &&
          Number.isInteger(action.id)) {
        state.analyzerNodes.add(action.id);
      }

      const routeType = routedActionType(action);
      if (routeType === START_CAPTURE) {
        state.captureActive = true;
      } else if (routeType === STOP_CAPTURE) {
        state.captureActive = false;
        stoppedCapture = true;
      }

      if (action?.type === REMOVE_NODE && state.analyzerNodes.has(action.id)) {
        if (state.captureActive) {
          state.deferredAnalyzerRemovals.add(action.id);
          changed = true;
          this.log(
            `[logic2-bridge:guard] deferred analyzer removal session=${key} ` +
            `node=${action.id} until capture stops`,
          );
          continue;
        }
        state.analyzerNodes.delete(action.id);
        state.deferredAnalyzerRemovals.delete(action.id);
      }
      actions.push(action);
    }

    if (stoppedCapture && !state.captureActive && state.deferredAnalyzerRemovals.size > 0) {
      for (const nodeId of state.deferredAnalyzerRemovals) {
        actions.push({ type: REMOVE_NODE, id: nodeId });
        state.analyzerNodes.delete(nodeId);
        this.log(
          `[logic2-bridge:guard] released analyzer removal session=${key} node=${nodeId}`,
        );
      }
      state.deferredAnalyzerRemovals.clear();
      changed = true;
    }

    if (!changed) return undefined;
    contents.actions = actions;
    return JSON.stringify(wrapped);
  }
}

module.exports = {
  GraphActionGuard,
  messageContents,
};
