'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { GraphActionGuard } = require('../lib/graph-action-guard.cjs');

const ADD_NODE = 'Saleae::Graph::GraphActions::AddNode';
const REMOVE_NODE = 'Saleae::Graph::GraphActions::RemoveNode';
const ROUTE_ACTION = 'Saleae::Graph::GraphActions::RouteAction';

function request(sessionId, actions) {
  return JSON.stringify({
    type: 'request',
    contents: {
      id: 1,
      type: 'Saleae::Graph::GraphActionData',
      runInParallel: false,
      actions,
      meta: { sessionId, destination: 'GraphServer' },
    },
  });
}

function captureAction(type) {
  return {
    type: ROUTE_ACTION,
    nodeId: 2,
    action: { type: `Saleae::Graph::LogicDevice::${type}` },
  };
}

function analyzerNode(id) {
  return { type: ADD_NODE, id, name: 'I2C', nodeType: 'Saleae::Graph::AnalyzerNode' };
}

function transformedActions(guard, message) {
  const transformed = guard.transform(message);
  return transformed ? JSON.parse(transformed).contents.actions : undefined;
}

test('defers analyzer removal during capture and releases it after StopCapture', () => {
  const logs = [];
  const guard = new GraphActionGuard({ log: line => logs.push(line) });
  assert.equal(guard.transform(request(4, [analyzerNode(10010)])), undefined);
  assert.equal(guard.transform(request(4, [captureAction('StartCapture')])), undefined);

  assert.deepEqual(transformedActions(guard, request(4, [{ type: REMOVE_NODE, id: 10010 }])), []);
  assert.deepEqual(transformedActions(guard, request(4, [captureAction('StopCapture')])), [
    captureAction('StopCapture'),
    { type: REMOVE_NODE, id: 10010 },
  ]);
  assert.equal(logs.filter(line => line.includes('deferred analyzer removal')).length, 1);
  assert.equal(logs.filter(line => line.includes('released analyzer removal')).length, 1);
});

test('coalesces duplicate analyzer cleanup while capture is active', () => {
  const guard = new GraphActionGuard({ log: () => {} });
  guard.transform(request(1, [analyzerNode(10011), captureAction('StartCapture')]));
  assert.deepEqual(transformedActions(guard, request(1, [{ type: REMOVE_NODE, id: 10011 }])), []);
  assert.deepEqual(transformedActions(guard, request(1, [{ type: REMOVE_NODE, id: 10011 }])), []);
  const actions = transformedActions(guard, request(1, [captureAction('StopCapture')]));
  assert.equal(actions.filter(action => action.type === REMOVE_NODE).length, 1);
});

test('allows normal analyzer removal while capture is stopped', () => {
  const guard = new GraphActionGuard({ log: () => {} });
  guard.transform(request(2, [analyzerNode(10012)]));
  assert.equal(guard.transform(request(2, [{ type: REMOVE_NODE, id: 10012 }])), undefined);
});

test('preserves an explicit StopCapture then analyzer removal sequence', () => {
  const guard = new GraphActionGuard({ log: () => {} });
  guard.transform(request(2, [analyzerNode(10015), captureAction('StartCapture')]));
  assert.equal(guard.transform(request(2, [
    captureAction('StopCapture'),
    { type: REMOVE_NODE, id: 10015 },
  ])), undefined);
});

test('does not change ordinary graph node removal during capture', () => {
  const guard = new GraphActionGuard({ log: () => {} });
  guard.transform(request(3, [
    { type: ADD_NODE, id: 10013, name: 'query', nodeType: 'Saleae::Graph::FrameDBQueryNode' },
    captureAction('StartCapture'),
  ]));
  assert.equal(guard.transform(request(3, [{ type: REMOVE_NODE, id: 10013 }])), undefined);
});

test('drops deferred cleanup state when its graph is destroyed', () => {
  const guard = new GraphActionGuard({ log: () => {} });
  guard.transform(request(5, [analyzerNode(10014), captureAction('StartCapture')]));
  transformedActions(guard, request(5, [{ type: REMOVE_NODE, id: 10014 }]));
  const deletion = JSON.stringify({
    type: 'request',
    contents: {
      id: 2,
      type: 'Saleae::Graph::DeleteGraphData',
      meta: { sessionId: 5, destination: 'GraphServer' },
    },
  });
  assert.equal(guard.transform(deletion), undefined);
  assert.equal(guard.transform(request(5, [captureAction('StopCapture')])), undefined);
});
