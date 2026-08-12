'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  PxlogicCaptureController,
  buildPxlogicHelperArguments,
  buildPxlogicPrepareArguments,
  describeDigitalTrigger,
  extractLogicRequests,
  parseThresholdVolts,
} = require('../lib/capture-controller.cjs');

test('passes the Bridge hardware threshold to PXLogic without rescaling', () => {
  const arguments_ = buildPxlogicHelperArguments({
    captureWindowMs: 1000,
    pxlogicDevice: 'usb:test',
  }, {
    enabledChannels: [4, 5],
    sampleRateHz: 50_000_000,
    thresholdVolts: 1.2,
  });
  assert.equal(arguments_[arguments_.indexOf('--vth') + 1], '1.2');
  assert.equal(arguments_.filter(argument => argument === '--skip-prepare').length, 1);
  assert.equal(arguments_.includes('--prepare-only'), false);
});

test('prepares the selected PXLogic once outside the capture process', () => {
  assert.deepEqual(buildPxlogicPrepareArguments({ pxlogicDevice: 'usb:test' }), [
    '--device',
    'usb:test',
    '--prepare-only',
  ]);
  assert.deepEqual(buildPxlogicPrepareArguments({}), ['--prepare-only']);
});

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

test('keeps the Bridge hardware threshold independent from Logic UI voltage', () => {
  const controller = new PxlogicCaptureController({
    enabledChannels: [0, 1, 2, 3],
    sampleRateHz: 25_000_000,
    thresholdVolts: 2.0,
    hardwareThresholdVolts: 1.12,
  }, {});
  controller.applySetting(4, {
    type: 'Saleae::Graph::LogicDevice::SetDigitalVoltageThreshold',
    thresholdDescription: '1.8 Volts',
  });
  assert.equal(controller.getSessionSettings(4).thresholdVolts, 1.8);
  assert.equal(controller.options.hardwareThresholdVolts, 1.12);
});
