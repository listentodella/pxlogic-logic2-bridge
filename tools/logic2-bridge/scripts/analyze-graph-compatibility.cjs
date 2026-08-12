#!/usr/bin/env node
'use strict';

const { analyzeGraph } = require('../lib/offline-compatibility.cjs');

function option(argv, name) {
  const index = argv.indexOf(name);
  return index === -1 ? undefined : argv[index + 1];
}

function main(argv) {
  if (argv.length === 0 || argv.includes('--help') || argv.includes('-h')) {
    console.log(
      'Usage: node scripts/analyze-graph-compatibility.cjs <GraphServer binary> ' +
      '[--logic-version VERSION] [--platform PLATFORM] [--architecture ARCH] ' +
      '[--cache FILE] [--force]',
    );
    process.exitCode = argv.length === 0 ? 2 : 0;
    return;
  }
  const graphPath = argv[0];
  const result = analyzeGraph({
    graphPath,
    logicVersion: option(argv, '--logic-version'),
    platform: option(argv, '--platform'),
    architecture: option(argv, '--architecture'),
    cachePath: option(argv, '--cache'),
    force: argv.includes('--force'),
  });
  console.log(JSON.stringify(result, null, 2));
}

try {
  main(process.argv.slice(2));
} catch (error) {
  console.error(`[logic2-bridge:compatibility] ${error.message}`);
  process.exitCode = 1;
}
