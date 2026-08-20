'use strict';

const fs = require('node:fs');
const path = require('node:path');

const GRAPH_ANALYZER_CLEANUP_FAILURE = Object.freeze({
  code: 'GRAPH_ANALYZER_CLEANUP_CRASH',
  message: 'Logic 2 图形服务在采集中清理协议分析器时异常退出。PXLogic 设备通常未损坏；请重新初始化 Bridge，诊断信息已保留。',
  recoveryAction: 'restart-bridge',
});

const GRAPH_LOG_POLL_INTERVAL_MS = 100;
const GRAPH_LOG_CONTEXT_BYTES = 64 * 1024;
const GRAPH_LOG_READ_BYTES = 4 * 1024 * 1024;

function classifyGraphServerFailure(lines) {
  const text = Array.isArray(lines) ? lines.join('\n') : String(lines || '');
  if (text.includes('simulation_provider.cpp:45') &&
      text.includes('removing an analyzer during a simulation')) {
    return { ...GRAPH_ANALYZER_CLEANUP_FAILURE };
  }
  return null;
}

function readTextTail(filePath, maxBytes = 64 * 1024) {
  try {
    const contents = fs.readFileSync(filePath);
    return contents.subarray(Math.max(0, contents.length - maxBytes)).toString('utf8');
  } catch {
    return null;
  }
}

function readCrashReportHeader(filePath) {
  try {
    const firstLine = fs.readFileSync(filePath, 'utf8').split(/\r?\n/, 1)[0];
    return firstLine ? JSON.parse(firstLine) : null;
  } catch {
    return null;
  }
}

function recentGraphHostCrashReports(
  homeDirectory,
  { platform = process.platform, maxReports = 3 } = {},
) {
  if (platform !== 'darwin' || !homeDirectory || maxReports <= 0) return [];
  const directory = path.join(homeDirectory, 'Library', 'Logs', 'DiagnosticReports');
  let entries;
  try {
    entries = fs.readdirSync(directory, { withFileTypes: true });
  } catch {
    return [];
  }
  return entries
    .filter(entry => entry.isFile() && /^graph-host-.*\.ips$/.test(entry.name))
    .map(entry => {
      const reportPath = path.join(directory, entry.name);
      try {
        const stat = fs.statSync(reportPath);
        const reportTail = readTextTail(reportPath);
        return {
          path: reportPath,
          modifiedAtUnixSeconds: Math.floor(stat.mtimeMs / 1000),
          sizeBytes: stat.size,
          header: readCrashReportHeader(reportPath),
          reportTail,
        };
      } catch {
        return null;
      }
    })
    .filter(Boolean)
    .sort((left, right) => right.modifiedAtUnixSeconds - left.modifiedAtUnixSeconds)
    .slice(0, maxReports);
}

function graphLogFileIdentity(stat) {
  // dev/ino are stable on macOS and let us recognize a log replacement even
  // when the replacement happens to have the same length as the old file.
  if (stat && (stat.dev !== undefined || stat.ino !== undefined)) {
    return `${stat.dev ?? ''}:${stat.ino ?? ''}`;
  }
  return null;
}

class GraphLogMonitor {
  constructor(
    filePath,
    {
      intervalMs = GRAPH_LOG_POLL_INTERVAL_MS,
      onFailure = () => {},
      fsModule = fs,
    } = {},
  ) {
    this.filePath = filePath;
    this.intervalMs = intervalMs;
    this.onFailure = onFailure;
    this.fs = fsModule;
    this.offset = null;
    this.identity = null;
    this.context = '';
    this.failureReported = false;
    this.timer = null;
  }

  start() {
    if (this.timer) return this;
    this.poll();
    this.timer = setInterval(() => this.poll(), this.intervalMs);
    this.timer.unref?.();
    return this;
  }

  stop() {
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
    return this;
  }

  poll() {
    let stat;
    try {
      stat = this.fs.statSync(this.filePath);
    } catch {
      return null;
    }

    const identity = graphLogFileIdentity(stat);
    if (this.offset === null ||
        (identity !== null && this.identity !== null && identity !== this.identity) ||
        stat.size < this.offset) {
      // Never replay an old assertion from a rotated/truncated graphio.log.
      this.offset = stat.size;
      this.identity = identity;
      this.context = '';
      return null;
    }
    this.identity = identity;
    if (stat.size === this.offset) return null;

    const start = this.offset;
    const length = Math.min(stat.size - start, GRAPH_LOG_READ_BYTES);
    let descriptor;
    let bytesRead;
    try {
      descriptor = this.fs.openSync(this.filePath, 'r');
      const buffer = Buffer.alloc(length);
      bytesRead = this.fs.readSync(descriptor, buffer, 0, length, start);
      const appended = buffer.subarray(0, bytesRead).toString('utf8');
      this.offset = start + bytesRead;
      this.context = `${this.context}${appended}`.slice(-GRAPH_LOG_CONTEXT_BYTES);
      const failure = classifyGraphServerFailure(this.context);
      if (!failure || this.failureReported) return null;
      this.failureReported = true;
      const event = {
        type: 'graphserver-failure',
        code: failure.code,
        detail: failure.message,
        recoveryAction: failure.recoveryAction,
      };
      this.onFailure(event);
      return event;
    } catch {
      return null;
    } finally {
      if (descriptor !== undefined) {
        try {
          this.fs.closeSync(descriptor);
        } catch {
          // The file can disappear during rotation; the next poll will resync.
        }
      }
    }
  }
}

module.exports = {
  GRAPH_ANALYZER_CLEANUP_FAILURE,
  GraphLogMonitor,
  classifyGraphServerFailure,
  recentGraphHostCrashReports,
};
