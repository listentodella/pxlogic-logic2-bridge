#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const bridgeRoot = path.resolve(__dirname, '..');
const repositoryRoot = path.resolve(bridgeRoot, '..', '..');
const tauriManifest = path.join(
  bridgeRoot,
  'tauri-client',
  'src-tauri',
  'Cargo.toml',
);

function parseArguments(argv) {
  const result = { reportPath: null };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--') {
      continue;
    } else if (argument === '--report') {
      const value = argv[index + 1];
      if (!value) throw new Error('--report requires a file path');
      result.reportPath = path.resolve(value);
      index += 1;
    } else {
      throw new Error(`Unknown delivery-check argument: ${argument}`);
    }
  }
  return result;
}

function buildChecks() {
  return [
    {
      id: 'bridge-node-check',
      label: 'Bridge JavaScript syntax',
      command: 'npm',
      args: ['run', 'check'],
      cwd: bridgeRoot,
    },
    {
      id: 'bridge-node-tests',
      label: 'Bridge protocol and conversion tests',
      command: 'npm',
      args: ['test'],
      cwd: bridgeRoot,
    },
    {
      id: 'pxlogic-rust-format',
      label: 'PXLogic Rust formatting',
      command: 'cargo',
      args: ['fmt', '--all', '--', '--check'],
      cwd: repositoryRoot,
    },
    {
      id: 'pxlogic-core-tests',
      label: 'PXLogic core tests',
      command: 'cargo',
      args: ['test', '-p', 'pxlogic-core', '--lib'],
      cwd: repositoryRoot,
    },
    {
      id: 'pxlogic-helper-check',
      label: 'PXLogic capture helper compile check',
      command: 'cargo',
      args: ['check', '--bin', 'usb_smoke'],
      cwd: repositoryRoot,
    },
    {
      id: 'tauri-rust-format',
      label: 'Tauri client formatting',
      command: 'cargo',
      args: ['fmt', '--manifest-path', tauriManifest, '--', '--check'],
      cwd: repositoryRoot,
    },
    {
      id: 'tauri-rust-tests',
      label: 'Tauri client tests',
      command: 'cargo',
      args: ['test', '--manifest-path', tauriManifest],
      cwd: repositoryRoot,
    },
  ];
}

function commandText(check) {
  return [check.command, ...check.args].join(' ');
}

function runCommand(check) {
  const started = Date.now();
  process.stdout.write(`\n[delivery-check] $ ${commandText(check)}\n`);
  const result = spawnSync(check.command, check.args, {
    cwd: check.cwd,
    encoding: 'utf8',
    env: process.env,
    maxBuffer: 32 * 1024 * 1024,
  });
  const output = `${result.stdout || ''}${result.stderr || ''}`;
  if (output) process.stdout.write(output);
  const passed = !result.error && result.status === 0;
  return {
    id: check.id,
    label: check.label,
    status: passed ? 'PASS' : 'FAIL',
    command: commandText(check),
    durationMs: Date.now() - started,
    exitCode: result.status,
    error: result.error ? result.error.message : null,
    outputTail: output.slice(-8 * 1024),
  };
}

function readJson(relativePath) {
  return JSON.parse(fs.readFileSync(path.join(repositoryRoot, relativePath), 'utf8'));
}

function readCargoVersion(manifestPath) {
  const manifest = fs.readFileSync(manifestPath, 'utf8');
  const packageSection = manifest.match(/\[package\]([\s\S]*?)(?:\n\[|$)/);
  const version = packageSection?.[1].match(/^version\s*=\s*"([^"]+)"/m);
  return version?.[1] || null;
}

function versionCheck() {
  const versions = {
    repository: readJson('package.json').version,
    bridge: readJson('tools/logic2-bridge/package.json').version,
    electronClient: readJson('tools/logic2-bridge/client/package.json').version,
    tauriPackage: readJson('tools/logic2-bridge/tauri-client/package.json').version,
    tauriConfig: readJson(
      'tools/logic2-bridge/tauri-client/src-tauri/tauri.conf.json',
    ).version,
    tauriCargo: readCargoVersion(tauriManifest),
  };
  const distinct = [...new Set(Object.values(versions).filter(Boolean))];
  return {
    id: 'version-alignment',
    label: 'Application version alignment',
    status: distinct.length === 1 ? 'PASS' : 'WARN',
    versions,
    detail: distinct.length === 1
      ? `All manifests report ${distinct[0]}`
      : `Manifest versions differ: ${distinct.join(', ')}`,
  };
}

function gitValue(args) {
  const result = spawnSync('git', args, {
    cwd: repositoryRoot,
    encoding: 'utf8',
  });
  return result.status === 0 ? result.stdout.trim() : null;
}

function provenance() {
  return {
    head: gitValue(['rev-parse', 'HEAD']),
    branch: gitValue(['branch', '--show-current']),
    upstream: gitValue([
      'rev-parse',
      '--abbrev-ref',
      '--symbolic-full-name',
      '@{upstream}',
    ]),
    dirtyPaths: (gitValue(['status', '--porcelain']) || '')
      .split(/\r?\n/)
      .filter(Boolean),
  };
}

function overallStatus(checks) {
  if (checks.some(check => check.status === 'FAIL')) return 'FAIL';
  if (checks.some(check => check.status === 'WARN')) return 'WARN';
  return 'PASS';
}

function writeReport(reportPath, report) {
  if (!reportPath) return;
  fs.mkdirSync(path.dirname(reportPath), { recursive: true });
  fs.writeFileSync(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`[delivery-check] report: ${reportPath}\n`);
}

function appendGitHubSummary(report) {
  const summaryPath = process.env.GITHUB_STEP_SUMMARY;
  if (!summaryPath) return;
  const rows = report.checks.map(check => (
    `| ${check.status} | ${check.label} | ${check.durationMs ?? '-'} |`
  ));
  fs.appendFileSync(summaryPath, [
    '## Bridge delivery self-check',
    '',
    `Overall: **${report.status}**`,
    '',
    '| Status | Check | Duration (ms) |',
    '| --- | --- | ---: |',
    ...rows,
    '',
    `Commit: \`${report.provenance.head || 'unknown'}\``,
    '',
  ].join('\n'));
}

function main(argv = process.argv.slice(2)) {
  const options = parseArguments(argv);
  const checks = [versionCheck()];
  for (const check of buildChecks()) checks.push(runCommand(check));
  const report = {
    schemaVersion: 1,
    generatedAt: new Date().toISOString(),
    platform: process.platform,
    architecture: process.arch,
    nodeVersion: process.version,
    provenance: provenance(),
    status: overallStatus(checks),
    checks,
  };
  writeReport(options.reportPath, report);
  appendGitHubSummary(report);
  process.stdout.write(`\n[delivery-check] overall: ${report.status}\n`);
  if (report.status === 'FAIL') process.exitCode = 1;
  return report;
}

module.exports = {
  buildChecks,
  main,
  overallStatus,
  parseArguments,
  versionCheck,
};

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`[delivery-check] ${error.message}`);
    process.exitCode = 1;
  }
}
