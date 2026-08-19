'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const {
  PxlogicCaptureController,
  bridgeEventLine,
  buildPxlogicHelperArguments,
  buildPxlogicPrepareArguments,
  classifyPxlogicHelperExit,
  describeDigitalTrigger,
  extractLogicRequests,
  parseThresholdVolts,
} = require('../lib/capture-controller.cjs');

test('classifies a unique ready USB address replacement as re-enumeration', () => {
  const failure = classifyPxlogicHelperExit('usb:16c0:05dc:3:8', 1, [{
    id: 'usb:16c0:05dc:3:10',
    vid: 0x16c0,
    pid: 0x05dc,
    ready: true,
  }]);
  assert.deepEqual(failure, {
    code: 'PXLOGIC_USB_REENUMERATED',
    detail: 'USB device changed address from usb:16c0:05dc:3:8 to usb:16c0:05dc:3:10',
    recoveryAction: 'rescan-and-restart',
  });
});

test('does not guess which device re-enumerated when matches are ambiguous', () => {
  const failure = classifyPxlogicHelperExit('usb:16c0:05dc:3:8', 1, [
    { id: 'usb:16c0:05dc:3:10', vid: 0x16c0, pid: 0x05dc, ready: true },
    { id: 'usb:16c0:05dc:4:2', vid: 0x16c0, pid: 0x05dc, ready: true },
  ]);
  assert.deepEqual(failure, {
    code: 'PXLOGIC_HELPER_EXITED',
    detail: 'helper exited with code 1',
  });
});

test('does not report re-enumeration while the selected USB address still exists', () => {
  const failure = classifyPxlogicHelperExit('usb:16c0:05dc:3:8', 1, [
    { id: 'usb:16c0:05dc:3:8', vid: 0x16c0, pid: 0x05dc, ready: false },
    { id: 'usb:16c0:05dc:3:10', vid: 0x16c0, pid: 0x05dc, ready: true },
  ]);
  assert.equal(failure.code, 'PXLOGIC_HELPER_EXITED');
});

test('keeps non-USB helper failures generic', () => {
  assert.deepEqual(classifyPxlogicHelperExit('fake:test', 2, [{
    id: 'usb:16c0:05dc:3:10', vid: 0x16c0, pid: 0x05dc, ready: true,
  }]), {
    code: 'PXLOGIC_HELPER_EXITED',
    detail: 'helper exited with code 2',
  });
});

test('serializes machine-readable bridge events for desktop recovery', () => {
  assert.equal(
    bridgeEventLine({
      type: 'capture-unavailable',
      code: 'PXLOGIC_HELPER_EXITED',
      recoveryAction: 'restart-bridge',
    }),
    '[logic2-bridge:event] {"type":"capture-unavailable","code":' +
      '"PXLOGIC_HELPER_EXITED","recoveryAction":"restart-bridge"}',
  );
});

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

test('records the Logic trigger description without enabling a PXLogic hardware trigger', () => {
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
    triggerDescription: 'D4 rising',
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
