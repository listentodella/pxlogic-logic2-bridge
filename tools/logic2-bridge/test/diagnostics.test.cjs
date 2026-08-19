'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const {
  classifyGraphServerFailure,
  recentGraphHostCrashReports,
} = require('../lib/diagnostics.cjs');

test('classifies the confirmed GraphServer analyzer cleanup assertion', () => {
  const failure = classifyGraphServerFailure([
    '[main] [critical] [simulation_provider.cpp:45] Assert: TODO: add support for removing an analyzer during a simulation',
  ]);
  assert.equal(failure.code, 'GRAPH_ANALYZER_CLEANUP_CRASH');
  assert.equal(failure.recoveryAction, 'restart-bridge');
  assert.match(failure.message, /PXLogic 设备通常未损坏/);
  assert.equal(classifyGraphServerFailure('Pipe with specified id does not exist'), null);
});

test('includes recent graph-host crash reports in diagnostic snapshots', t => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), 'pxlogic-diagnostics-'));
  t.after(() => fs.rmSync(home, { recursive: true, force: true }));
  const directory = path.join(home, 'Library', 'Logs', 'DiagnosticReports');
  fs.mkdirSync(directory, { recursive: true });
  const reportPath = path.join(directory, 'graph-host-2026-08-19-172435.ips');
  fs.writeFileSync(reportPath, '{"app_name":"graph-host","incident_id":"test"}\nstack');
  const reports = recentGraphHostCrashReports(home, { platform: 'darwin' });
  assert.equal(reports.length, 1);
  assert.equal(reports[0].path, reportPath);
  assert.equal(reports[0].header.incident_id, 'test');
  assert.match(reports[0].reportTail, /stack/);
});
