'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  PxlogicCaptureController,
  describeDigitalTrigger,
  extractLogicRequests,
  parseThresholdVolts,
} = require('../lib/capture-controller.cjs');

test('extracts Logic device controls without requiring a private app', () => {
  const requests = extractLogicRequests(JSON.stringify({
    type: 'request',
    contents: {
      type: 'Saleae::Graph::GraphActionData',
      meta: { sessionId: 7 },
      actions: [{
        type: 'Saleae::Graph::GraphActions::RouteAction',
        action: { type: 'Saleae::Graph::LogicDevice::SetSampleRate', digital: 50_000_000 },
      }],
    },
  }));
  assert.deepEqual(requests, [{
    sessionId: 7,
    action: { type: 'Saleae::Graph::LogicDevice::SetSampleRate', digital: 50_000_000 },
  }]);
});

test('keeps nominal voltage and GraphServer trigger semantics', () => {
  assert.equal(parseThresholdVolts('1.8 Volts'), 1.8);
  assert.equal(parseThresholdVolts('3.3 Volts'), 3.3);
  assert.equal(parseThresholdVolts('Custom'), undefined);
  assert.equal(describeDigitalTrigger({
    enabled: true,
    trigger: { type: 'EdgeTrigger', edgeChannel: 4, edgeType: 'Rising' },
  }), 'D4 rising');
  assert.match(describeDigitalTrigger({
    enabled: true,
    trigger: {
      type: 'PulseTrigger', pulseChannel: 2, pulseType: 'Positive',
      durationRange: { begin: 1e-7, end: 2e-7 },
    },
  }), /^D2 positive pulse/);
});

test('observes a Logic trigger without storing PXLogic hardware trigger state', () => {
  const controller = new PxlogicCaptureController({
    enabledChannels: [0, 1, 2, 3, 4],
    sampleRateHz: 50_000_000,
    thresholdVolts: 1.8,
  }, {});
  controller.applySetting(3, {
    type: 'Saleae::Graph::DigitalTriggerSettingsData',
    enabled: true,
    trigger: { type: 'EdgeTrigger', edgeChannel: 4, edgeType: 'Rising' },
  });
  assert.deepEqual(controller.getSessionSettings(3), {
    enabledChannels: [0, 1, 2, 3, 4],
    sampleRateHz: 50_000_000,
    thresholdVolts: 1.8,
    logicSoftwareGlitchFilterWidths: {},
  });
});
