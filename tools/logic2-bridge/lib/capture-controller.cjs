'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawn } = require('node:child_process');
const {
  INJECTION_FRAME,
  createInjectionConfig,
  createInjectionFrame,
  encodePxlogicCrossChunk,
  normalizeEnabledChannels,
} = require('./logic-format.cjs');

function createLineReader(stream, onLine) {
  let pending = '';
  stream.setEncoding('utf8');
  stream.on('data', chunk => {
    pending += chunk;
    const lines = pending.split(/\r?\n/);
    pending = lines.pop() || '';
    for (const line of lines) {
      if (line) onLine(line);
    }
  });
  stream.on('end', () => {
    if (pending) onLine(pending);
  });
}

function bridgeEventLine(event) {
  return `[logic2-bridge:event] ${JSON.stringify(event)}`;
}

function emitBridgeEvent(event) {
  console.error(bridgeEventLine(event));
}

function parseThresholdVolts(description) {
  const match = String(description ?? '').match(/[-+]?(?:\d+(?:\.\d*)?|\.\d+)/);
  if (!match) return undefined;
  const volts = Number(match[0]);
  if (!Number.isFinite(volts) || volts < 0 || volts > 6.668) return undefined;
  return volts;
}

function physicalChannelSpan(enabledChannels) {
  return Math.max(...normalizeEnabledChannels(enabledChannels)) + 1;
}

const PXVIEW_STREAM_MODES = Object.freeze({
  super: Object.freeze({
    0: [['STREAM_LOGIC50x32', 32, 50_000_000], ['STREAM_LOGIC125x16', 16, 125_000_000], ['STREAM_LOGIC250x8', 8, 250_000_000], ['STREAM_LOGIC500x4', 4, 500_000_000], ['STREAM_LOGIC1000x2', 2, 1_000_000_000]],
    1: [['STREAM_LOGIC125x16', 16, 125_000_000], ['STREAM_LOGIC250x8', 8, 250_000_000], ['STREAM_LOGIC500x4', 4, 500_000_000], ['STREAM_LOGIC1000x2', 2, 1_000_000_000]],
    2: [['STREAM_LOGIC125x16', 16, 125_000_000], ['STREAM_LOGIC250x8', 8, 250_000_000], ['STREAM_LOGIC500x4', 4, 500_000_000]],
    3: [['STREAM_LOGIC125x16', 16, 125_000_000], ['STREAM_LOGIC250x8', 8, 250_000_000]],
  }),
  high: Object.freeze({
    0: [['STREAM_LOGIC200x1', 1, 200_000_000], ['STREAM_LOGIC100x2', 2, 100_000_000], ['STREAM_LOGIC50x4', 4, 50_000_000], ['STREAM_LOGIC25x8', 8, 25_000_000], ['STREAM_LOGIC10x16', 16, 10_000_000], ['STREAM_LOGIC5x32', 32, 5_000_000]],
    1: [['STREAM_LOGIC200x1', 1, 200_000_000], ['STREAM_LOGIC100x2', 2, 100_000_000], ['STREAM_LOGIC50x4', 4, 50_000_000], ['STREAM_LOGIC25x8', 8, 25_000_000], ['STREAM_LOGIC10x16', 16, 10_000_000]],
    2: [['STREAM_LOGIC200x1', 1, 200_000_000], ['STREAM_LOGIC100x2', 2, 100_000_000], ['STREAM_LOGIC50x4', 4, 50_000_000], ['STREAM_LOGIC25x8', 8, 25_000_000], ['STREAM_LOGIC10x16', 16, 10_000_000]],
    3: [['STREAM_LOGIC200x1', 1, 200_000_000], ['STREAM_LOGIC100x2', 2, 100_000_000], ['STREAM_LOGIC50x4', 4, 50_000_000], ['STREAM_LOGIC25x8', 8, 25_000_000], ['STREAM_LOGIC10x16', 16, 10_000_000]],
  }),
});

function pxviewStreamModes(usbSpeed, logicMode) {
  const speed = String(usbSpeed || '').toLowerCase() === 'super' ? 'super' : 'high';
  return PXVIEW_STREAM_MODES[speed][Number.isInteger(logicMode) ? logicMode : 0] ||
    PXVIEW_STREAM_MODES[speed][0];
}

function resolvePxlogicStreamPlan({ usbSpeed, logicMode, enabledChannels, sampleRateHz }) {
  const channels = normalizeEnabledChannels(enabledChannels);
  const channelSpan = physicalChannelSpan(channels);
  const modes = pxviewStreamModes(usbSpeed, logicMode);
  const candidates = modes
    .filter(([, physicalChannels, maxRate]) => physicalChannels >= channelSpan && sampleRateHz <= maxRate)
    .sort((left, right) => left[1] - right[1] || left[2] - right[2]);
  const selected = candidates[0] || modes
    .filter(([, physicalChannels]) => physicalChannels >= channelSpan)
    .sort((left, right) => right[2] - left[2] || left[1] - right[1])[0];
  if (!selected) {
    return {
      supported: false,
      reason: `PXView Stream 不支持物理通道跨度 ${channelSpan}`,
      channelSpan,
      enabledLaneCount: channels.length,
      availableModes: modes.map(([id, physicalChannels, maxRate]) => ({ id, physicalChannels, maxRate })),
    };
  }
  const [, physicalChannels, maxRate] = selected;
  return {
    supported: sampleRateHz <= maxRate,
    reason: sampleRateHz <= maxRate ? null : `请求采样率超过 ${selected[0]} 上限`,
    channelSpan,
    enabledLaneCount: channels.length,
    mode: selected[0],
    modePhysicalChannels: physicalChannels,
    requestedSampleRateHz: sampleRateHz,
    effectiveSampleRateHz: Math.min(sampleRateHz, maxRate),
    modeMaxSampleRateHz: maxRate,
    availableModes: modes.map(([id, modeChannels, modeMaxRate]) => ({
      id, physicalChannels: modeChannels, maxRate: modeMaxRate,
    })),
  };
}

function extractLogicRequests(message) {
  let wrapped;
  try {
    wrapped = typeof message === 'string' ? JSON.parse(message) : message;
  } catch {
    return [];
  }
  const contents = wrapped?.type === 'request' ? wrapped.contents : wrapped;
  if (!contents) return [];
  const sessionId = contents.meta?.sessionId;
  const candidates = contents.type === 'Saleae::Graph::GraphActionData' && Array.isArray(contents.actions)
    ? contents.actions.map(action =>
        action?.type === 'Saleae::Graph::GraphActions::RouteAction' ? action.action : undefined)
    : [contents];
  return candidates
    .filter(action => action && typeof action.type === 'string' &&
      (action.type.startsWith('Saleae::Graph::LogicDevice::') ||
       action.type === 'Saleae::Graph::DigitalTriggerSettingsData'))
    .map(action => ({ sessionId, action }));
}

function describeDigitalTrigger(action) {
  if (!action?.enabled || !action.trigger) return 'off';
  const trigger = action.trigger;
  if (trigger.type === 'EdgeTrigger') {
    return `D${trigger.edgeChannel} ${String(trigger.edgeType || 'edge').toLowerCase()}`;
  }
  if (trigger.type === 'PulseTrigger') {
    const range = trigger.durationRange || {};
    return `D${trigger.pulseChannel} ${String(trigger.pulseType || 'pulse').toLowerCase()} ` +
      `pulse ${range.begin ?? '?'}..${range.end ?? '?'} s`;
  }
  return String(trigger.type || 'unknown');
}

function requirePath(target, label) {
  if (!fs.existsSync(target)) throw new Error(`${label} was not found: ${target}`);
}

function buildPxlogicHelperArguments(options, captureSettings) {
  const { enabledChannels, sampleRateHz, thresholdVolts } = captureSettings;
  if (!Number.isFinite(thresholdVolts)) {
    throw new Error('PXLogic hardware threshold is not configured');
  }
  const channelSpan = physicalChannelSpan(enabledChannels);
  const helperArguments = [
    '--skip-prepare',
    '--live',
    '--live-cross-only',
    '--mode',
    'stream',
    '--rate',
    String(sampleRateHz),
    '--vth',
    String(thresholdVolts),
    '--ms',
    String(options.captureWindowMs),
    '--channels',
    String(channelSpan),
    '--enabled-channels',
    enabledChannels.join(','),
    '--decode-cross',
    '--glitch-filter',
  ];
  if (options.pxlogicDevice) helperArguments.unshift('--device', options.pxlogicDevice);
  if (options.pxlogicSerialNumber) {
    helperArguments.unshift('--device-serial', options.pxlogicSerialNumber);
  }
  return helperArguments;
}

function buildPxlogicPrepareArguments(options) {
  const helperArguments = ['--prepare-only'];
  if (options.pxlogicDevice) helperArguments.unshift('--device', options.pxlogicDevice);
  if (options.pxlogicSerialNumber) {
    helperArguments.unshift('--device-serial', options.pxlogicSerialNumber);
  }
  return helperArguments;
}

function parseUsbDeviceId(deviceId) {
  const match = String(deviceId || '').match(
    /^usb:([0-9a-f]{4}):([0-9a-f]{4}):(\d+):(\d+)$/i,
  );
  if (!match) return null;
  return {
    vid: Number.parseInt(match[1], 16),
    pid: Number.parseInt(match[2], 16),
    bus: Number(match[3]),
    address: Number(match[4]),
  };
}

function classifyPxlogicHelperExit(deviceId, code, devices = []) {
  const previous = parseUsbDeviceId(deviceId);
  if (previous && !devices.some(device => device.id === deviceId)) {
    const replacements = devices.filter(device =>
      device.ready === true &&
      Number(device.vid) === previous.vid &&
      Number(device.pid) === previous.pid &&
      device.id !== deviceId,
    );
    if (replacements.length === 1) {
      const replacement = replacements[0];
      return {
        code: 'PXLOGIC_USB_REENUMERATED',
        detail: `USB device changed address from ${deviceId} to ${replacement.id}`,
        recoveryAction: 'rescan-and-restart',
      };
    }
  }
  return { code: 'PXLOGIC_HELPER_EXITED', detail: `helper exited with code ${code}` };
}

function scanPxlogicDevices(options) {
  return new Promise((resolve, reject) => {
    const scanner = spawn(options.pxlogicHelper, ['--list-json'], {
      cwd: path.dirname(options.pxlogicHelper),
      env: {
        ...process.env,
        PXLOGIC_BITSTREAM_DIR: options.bitstreams,
        PXLOGIC_MCU_FIRMWARE: options.firmware,
      },
      stdio: ['ignore', 'pipe', 'pipe'],
      windowsHide: process.platform === 'win32',
    });
    let stdout = '';
    let stderr = '';
    scanner.stdout.setEncoding('utf8');
    scanner.stderr.setEncoding('utf8');
    scanner.stdout.on('data', chunk => { stdout += chunk; });
    scanner.stderr.on('data', chunk => { stderr += chunk; });
    scanner.once('error', reject);
    scanner.once('close', code => {
      if (code !== 0) {
        reject(new Error(stderr.trim() || `device scan exited with code ${code}`));
        return;
      }
      try {
        const devices = JSON.parse(stdout);
        if (!Array.isArray(devices)) throw new Error('device scan did not return an array');
        resolve(devices);
      } catch (error) {
        reject(new Error(`invalid device scan response: ${error.message}`));
      }
    });
  });
}

async function diagnosePxlogicHelperExit(options, code) {
  try {
    const devices = await scanPxlogicDevices(options);
    return classifyPxlogicHelperExit(options.pxlogicDevice, code, devices);
  } catch (error) {
    console.error(`[logic2-bridge:pxlogic] post-exit USB scan failed: ${error.message}`);
    return classifyPxlogicHelperExit(options.pxlogicDevice, code);
  }
}

function preparePxlogicDevice(options) {
  requirePath(options.pxlogicHelper, 'PXLogic USB helper');
  requirePath(options.bitstreams, 'PXLogic bitstream directory');
  requirePath(options.firmware, 'PXLogic MCU firmware');

  const helperArguments = buildPxlogicPrepareArguments(options);
  console.error(
    `[logic2-bridge:pxlogic] preparing FPGA once for this Bridge session: ` +
    `${options.pxlogicDevice || 'first ready device'}`,
  );
  const helper = spawn(options.pxlogicHelper, helperArguments, {
    cwd: path.dirname(options.pxlogicHelper),
    env: {
      ...process.env,
      PXLOGIC_BITSTREAM_DIR: options.bitstreams,
      PXLOGIC_MCU_FIRMWARE: options.firmware,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: process.platform === 'win32',
  });
  createLineReader(helper.stdout, line => {
    console.error(`[logic2-bridge:pxlogic-prepare] ${line}`);
  });
  createLineReader(helper.stderr, line => {
    console.error(`[logic2-bridge:pxlogic-prepare] ${line}`);
  });

  return new Promise((resolve, reject) => {
    helper.once('error', reject);
    helper.once('close', (code, signal) => {
      if (code === 0) {
        console.error(
          '[logic2-bridge:pxlogic] FPGA prepared; Start/Stop will not upload bitstreams again',
        );
        resolve();
        return;
      }
      reject(new Error(
        `PXLogic FPGA prepare failed (${signal || `exit ${code}`}); ` +
        'capture was not enabled',
      ));
    });
  });
}

function startPxlogicFeeder(options, host, captureSettings) {
  requirePath(options.pxlogicHelper, 'PXLogic USB helper');
  requirePath(options.bitstreams, 'PXLogic bitstream directory');
  requirePath(options.firmware, 'PXLogic MCU firmware');

  const { enabledChannels, sampleRateHz, thresholdVolts, triggerDescription } = captureSettings;
  const channelSpan = physicalChannelSpan(enabledChannels);
  host.stdin.write(createInjectionConfig(enabledChannels));
  // PXLogic always uses its recommended one-sample source-side filter.
  // Logic's UI filter and all digital triggers remain GraphServer operations.
  const helperArguments = buildPxlogicHelperArguments(options, captureSettings);

  const helper = spawn(options.pxlogicHelper, helperArguments, {
    cwd: path.dirname(options.pxlogicHelper),
    env: {
      ...process.env,
      PXLOGIC_BITSTREAM_DIR: options.bitstreams,
      PXLOGIC_MCU_FIRMWARE: options.firmware,
    },
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: process.platform === 'win32',
  });

  let stopRequested = false;
  let failure;
  let stdoutPaused = false;
  let printedCaptureSettings = false;
  let printedProfile = false;
  let crossChunks = 0;
  let convertedBytes = 0;
  let lastProgressEventAt = 0;
  let resolveDone;
  const done = new Promise(resolve => {
    resolveDone = resolve;
  });

  const resumeStdout = () => {
    if (!stdoutPaused) return;
    stdoutPaused = false;
    helper.stdout.resume();
  };
  host.stdin.on('drain', resumeStdout);

  createLineReader(helper.stdout, line => {
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      const usefulDiagnostic =
        /^(devices:|selected:|\s+usb:|\s+label:|\s+usb_speed:)/.test(line) ||
        /^\[(prepare|firmware):(info|warn|error)\]/.test(line) ||
        /^\[[^:]+:(warn|error)\]/.test(line) ||
        (!printedProfile && /^\[profile:info\] matched /.test(line)) ||
        (!printedCaptureSettings && /^\[capture:info\] settings:/.test(line));
      if (usefulDiagnostic) console.error(`[logic2-bridge:pxlogic] ${line}`);
      if (/^\[profile:info\] matched /.test(line)) printedProfile = true;
      if (/^\[capture:info\] settings:/.test(line)) printedCaptureSettings = true;
      return;
    }

    if (event.type === 'started') {
      const metadata = event.metadata || {};
      const effectiveRate = metadata.sample_rate_hz || sampleRateHz;
      const effectiveChannels = metadata.enabled_channels || enabledChannels;
      if (effectiveRate !== sampleRateHz) {
        const detail = `Logic=${sampleRateHz} Hz, PXLogic=${effectiveRate} Hz`;
        failure = { code: 'PXLOGIC_RATE_MISMATCH', detail };
        console.error(`[logic2-bridge:pxlogic] refusing rate mismatch: ${detail}`);
        if (helper.exitCode === null) helper.kill('SIGTERM');
        return;
      }
      if (effectiveChannels.length !== enabledChannels.length ||
          effectiveChannels.some((channel, index) => channel !== enabledChannels[index])) {
        const detail = `expected ${enabledChannels.join(',')}, received ${effectiveChannels.join(',')}`;
        failure = { code: 'PXLOGIC_CHANNEL_MISMATCH', detail };
        console.error(`[logic2-bridge:pxlogic] refusing channel mismatch: ${detail}`);
        if (helper.exitCode === null) helper.kill('SIGTERM');
        return;
      }
      console.error(
        `[logic2-bridge:pxlogic] started rate=${effectiveRate} span=` +
        `${metadata.channel_count || channelSpan} channels=${effectiveChannels.join(',')}`,
      );
      emitBridgeEvent({
        type: 'capture-started',
        sampleRateHz: effectiveRate,
        enabledChannels: effectiveChannels,
        channelSpan: metadata.channel_count || channelSpan,
        thresholdVolts,
        triggerDescription,
      });
      return;
    }

    if (event.type === 'window') {
      const index = event.windowIndex || 0;
      if (index <= 2 || index % 64 === 0) {
        console.error(`[logic2-bridge:pxlogic] stream window=${index}`);
      }
      return;
    }
    if (event.type === 'session') {
      emitBridgeEvent({
        type: 'capture-progress',
        crossChunks,
        convertedBytes,
        windowCount: event.windowCount || 0,
        sampleCount: event.sampleCount || 0,
      });
      return;
    }
    if (event.type !== 'cross') return;

    const eventChannels = event.enabledChannels || enabledChannels;
    if (eventChannels.length !== enabledChannels.length ||
        eventChannels.some((channel, index) => channel !== enabledChannels[index])) {
      failure = {
        code: 'PXLOGIC_CHANNEL_MAPPING_CHANGED',
        detail: 'cross chunk channel mapping changed during capture',
      };
      console.error('[logic2-bridge:pxlogic] cross chunk channel mapping changed during capture');
      if (helper.exitCode === null) helper.kill('SIGTERM');
      return;
    }

    try {
      const cross = Buffer.from(event.data, 'base64');
      const logicData = encodePxlogicCrossChunk(cross, enabledChannels);
      crossChunks += 1;
      convertedBytes += logicData.length;
      const frame = createInjectionFrame(INJECTION_FRAME.DATA, logicData);
      if (!host.stdin.write(frame) && !stdoutPaused) {
        stdoutPaused = true;
        helper.stdout.pause();
      }
      if (crossChunks <= 3 || crossChunks % 1024 === 0) {
        console.error(
          `[logic2-bridge:pxlogic] chunk=${crossChunks} cross=${cross.length} ` +
          `logic=${logicData.length} total=${convertedBytes}`,
        );
      }
      const now = Date.now();
      if (crossChunks === 1 || now - lastProgressEventAt >= 500) {
        lastProgressEventAt = now;
        emitBridgeEvent({ type: 'capture-progress', crossChunks, convertedBytes });
      }
    } catch (error) {
      failure = { code: 'PXLOGIC_CONVERSION_FAILED', detail: error.message };
      console.error(`[logic2-bridge:pxlogic] conversion failed: ${error.message}`);
      if (helper.exitCode === null) helper.kill('SIGTERM');
    }
  });

  createLineReader(helper.stderr, line => {
    console.error(`[logic2-bridge:pxlogic-helper] ${line}`);
  });
  helper.once('error', error => {
    failure = { code: 'PXLOGIC_HELPER_START_FAILED', detail: error.message };
    console.error(`[logic2-bridge:pxlogic] helper failed to start: ${error.message}`);
    if (host.exitCode === null) host.kill('SIGTERM');
  });
  helper.once('close', async code => {
    host.stdin.removeListener('drain', resumeStdout);
    if (!host.stdin.destroyed) {
      host.stdin.write(createInjectionFrame(INJECTION_FRAME.END));
    }
    console.error(
      `[logic2-bridge:pxlogic] helper exited code=${code} chunks=${crossChunks} bytes=${convertedBytes}`,
    );
    if (!stopRequested && code !== 0 && !failure) {
      failure = await diagnosePxlogicHelperExit(options, code);
    }
    emitBridgeEvent({
      type: 'capture-ended',
      status: failure ? 'error' : 'stopped',
      crossChunks,
      convertedBytes,
      failed: Boolean(failure),
    });
    resolveDone({ code, failure });
  });

  console.error(
    `[logic2-bridge:pxlogic] starting ${options.pxlogicHelper} at ${sampleRateHz} Hz ` +
    `span=${channelSpan} on D${enabledChannels.join(',D')}`,
  );

  return {
    done,
    async stop() {
      if (stopRequested) return done;
      stopRequested = true;
      if (helper.exitCode !== null) return done;
      if (!helper.stdin.destroyed) helper.stdin.write('stop\n');
      setTimeout(() => {
        if (helper.exitCode === null) helper.kill('SIGTERM');
      }, 3000).unref();
      return done;
    },
  };
}

class PxlogicCaptureController {
  constructor(options, host) {
    this.options = options;
    this.host = host;
    this.sessions = new Map();
    this.activeFeeder = null;
    this.activeSessionId = undefined;
    this.captureUnavailableReason = undefined;
  }

  emitCapturePlan(sessionId, settings) {
    const plan = resolvePxlogicStreamPlan({
      usbSpeed: this.options.pxlogicUsbSpeed,
      logicMode: this.options.pxlogicLogicMode,
      enabledChannels: settings.enabledChannels,
      sampleRateHz: settings.sampleRateHz,
    });
    emitBridgeEvent({
      type: 'capture-plan',
      sessionId: sessionId ?? 'default',
      logicSampleRateHz: settings.sampleRateHz,
      enabledChannels: settings.enabledChannels,
      pxlogicUsbSpeed: this.options.pxlogicUsbSpeed,
      pxlogicLogicMode: this.options.pxlogicLogicMode,
      ...plan,
    });
  }

  getSessionSettings(sessionId) {
    const key = String(sessionId ?? 'default');
    let settings = this.sessions.get(key);
    if (!settings) {
      settings = {
        enabledChannels: [...this.options.enabledChannels],
        sampleRateHz: this.options.sampleRateHz,
        thresholdVolts: this.options.thresholdVolts,
        triggerDescription: 'off',
        logicSoftwareGlitchFilterWidths: {},
      };
      this.sessions.set(key, settings);
    }
    return settings;
  }

  applySetting(sessionId, action) {
    const settings = this.getSessionSettings(sessionId);
    if (action.type === 'Saleae::Graph::LogicDevice::EnableChannels' &&
        Array.isArray(action.channelStates)) {
      const enabled = new Set(settings.enabledChannels);
      for (const state of action.channelStates) {
        if (state?.channel?.type !== 'Digital') continue;
        const index = state.channel.index;
        if (!Number.isInteger(index) || index < 0 || index > 15) continue;
        if (state.enabled) enabled.add(index);
        else enabled.delete(index);
      }
      settings.enabledChannels = [...enabled].sort((left, right) => left - right);
      console.error(
        `[logic2-bridge:control] session=${sessionId ?? 'default'} channels=` +
        (settings.enabledChannels.length ? `D${settings.enabledChannels.join(',D')}` : 'none'),
      );
      this.emitCapturePlan(sessionId, settings);
    } else if (action.type === 'Saleae::Graph::LogicDevice::SetSampleRate' &&
               Number.isInteger(action.digital) && action.digital > 0) {
      settings.sampleRateHz = action.digital;
      console.error(
        `[logic2-bridge:control] session=${sessionId ?? 'default'} ` +
        `digital-rate=${settings.sampleRateHz}`,
      );
      this.emitCapturePlan(sessionId, settings);
    } else if (action.type === 'Saleae::Graph::LogicDevice::SetDigitalVoltageThreshold') {
      settings.thresholdVolts = parseThresholdVolts(action.thresholdDescription);
      const hardwareThreshold = this.options.hardwareThresholdVolts ?? settings.thresholdVolts;
      const threshold = Number.isFinite(hardwareThreshold)
        ? `${hardwareThreshold} V`
        : 'unsupported';
      console.error(
        `[logic2-bridge:control] session=${sessionId ?? 'default'} ` +
        `logic-level=${settings.thresholdVolts ?? 'unsupported'} V ` +
        `pxlogic-threshold=${threshold}`,
      );
    } else if (action.type === 'Saleae::Graph::DigitalTriggerSettingsData') {
      settings.triggerDescription = describeDigitalTrigger(action);
      console.error(
        `[logic2-bridge:control] Logic digital-trigger=${settings.triggerDescription} ` +
        '(GraphServer only)',
      );
    } else if (action.type === 'Saleae::Graph::LogicDevice::StartCapture') {
      settings.logicSoftwareGlitchFilterWidths = {
        ...(action.digitalChannelMaxGlitchWidth || {}),
      };
      const widths = Object.entries(settings.logicSoftwareGlitchFilterWidths)
        .filter(([, width]) => Number(width) > 0)
        .map(([channel, width]) => `D${channel}=${width}`)
        .join(',');
      console.error(
        `[logic2-bridge:control] Logic software-glitch-filter=${widths || 'off'} ` +
        '(GraphServer only)',
      );
    }
  }

  async startCapture(sessionId) {
    if (this.activeFeeder) await this.stopCapture(this.activeSessionId);
    if (this.captureUnavailableReason) {
      console.error(
        `[logic2-bridge:control] refusing capture: ${this.captureUnavailableReason}; ` +
        'restart PXLogic Bridge to prepare the device again',
      );
      return;
    }
    const settings = this.getSessionSettings(sessionId);
    if (settings.enabledChannels.length === 0) {
      console.error('[logic2-bridge:control] StartCapture has no enabled digital channels');
      return;
    }
    const captureSettings = {
      enabledChannels: [...settings.enabledChannels],
      sampleRateHz: settings.sampleRateHz,
      thresholdVolts: this.options.hardwareThresholdVolts ?? settings.thresholdVolts,
      triggerDescription: settings.triggerDescription,
    };
    const plan = resolvePxlogicStreamPlan({
      usbSpeed: this.options.pxlogicUsbSpeed,
      logicMode: this.options.pxlogicLogicMode,
      enabledChannels: captureSettings.enabledChannels,
      sampleRateHz: captureSettings.sampleRateHz,
    });
    if (plan.effectiveSampleRateHz) captureSettings.sampleRateHz = plan.effectiveSampleRateHz;
    console.error(
      `[logic2-bridge:control] StartCapture session=${sessionId ?? 'default'} ` +
      `rate=${captureSettings.sampleRateHz} ` +
      `channels=D${captureSettings.enabledChannels.join(',D')} ` +
      `pxlogic-threshold=${captureSettings.thresholdVolts} V ` +
      'pxlogic-hardware-glitch-filter=1T pxlogic-hardware-trigger=off',
    );
    emitBridgeEvent({
      type: 'capture-starting',
      logicSampleRateHz: settings.sampleRateHz,
      sampleRateHz: captureSettings.sampleRateHz,
      enabledChannels: captureSettings.enabledChannels,
      thresholdVolts: captureSettings.thresholdVolts,
      triggerDescription: captureSettings.triggerDescription,
      pxlogicUsbSpeed: this.options.pxlogicUsbSpeed,
      pxlogicLogicMode: this.options.pxlogicLogicMode,
      ...plan,
    });
    const feeder = startPxlogicFeeder(this.options, this.host, captureSettings);
    this.activeFeeder = feeder;
    this.activeSessionId = sessionId;
    feeder.done.then(result => {
      if (result.failure) {
        this.captureUnavailableReason = result.failure.detail;
        console.error(
          '[logic2-bridge:control] PXLogic capture disabled for the rest of this Bridge session; ' +
          'automatic FPGA reconfiguration is intentionally blocked',
        );
        emitBridgeEvent({
          type: 'capture-unavailable',
          code: result.failure.code,
          detail: result.failure.detail,
          recoveryAction: result.failure.recoveryAction || 'restart-bridge',
        });
      }
      if (this.activeFeeder === feeder) {
        this.activeFeeder = null;
        this.activeSessionId = undefined;
      }
    });
  }

  async stopCapture(sessionId) {
    if (!this.activeFeeder) return;
    const feeder = this.activeFeeder;
    console.error(`[logic2-bridge:control] StopCapture session=${sessionId ?? 'default'}`);
    await feeder.stop();
    if (this.activeFeeder === feeder) {
      this.activeFeeder = null;
      this.activeSessionId = undefined;
    }
  }

  async observeRequest(message) {
    for (const { sessionId, action } of extractLogicRequests(message)) {
      this.applySetting(sessionId, action);
      if (action.type === 'Saleae::Graph::LogicDevice::StartCapture') {
        await this.startCapture(sessionId);
      } else if (action.type === 'Saleae::Graph::LogicDevice::StopCapture') {
        setImmediate(() => {
          void this.stopCapture(sessionId).catch(error => {
            console.error(`[logic2-bridge:control] PXLogic stop failed: ${error.message}`);
          });
        });
      }
    }
  }

  async shutdown() {
    await this.stopCapture(this.activeSessionId);
  }
}

module.exports = {
  PxlogicCaptureController,
  bridgeEventLine,
  buildPxlogicHelperArguments,
  buildPxlogicPrepareArguments,
  classifyPxlogicHelperExit,
  createLineReader,
  describeDigitalTrigger,
  emitBridgeEvent,
  extractLogicRequests,
  parseThresholdVolts,
  physicalChannelSpan,
  pxviewStreamModes,
  resolvePxlogicStreamPlan,
  preparePxlogicDevice,
};
