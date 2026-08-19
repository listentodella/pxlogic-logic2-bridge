'use strict';

const fs = require('node:fs');
const path = require('node:path');

const GRAPH_ANALYZER_CLEANUP_FAILURE = Object.freeze({
  code: 'GRAPH_ANALYZER_CLEANUP_CRASH',
  message: 'Logic 2 图形服务在采集中清理协议分析器时异常退出。PXLogic 设备通常未损坏；请重新初始化 Bridge，诊断信息已保留。',
  recoveryAction: 'restart-bridge',
});

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

module.exports = {
  GRAPH_ANALYZER_CLEANUP_FAILURE,
  classifyGraphServerFailure,
  recentGraphHostCrashReports,
};
