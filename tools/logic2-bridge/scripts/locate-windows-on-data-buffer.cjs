#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const locator = require('../lib/windows-hook-locator.cjs');
const {
  METHOD_NAME,
  METHOD_SIGNATURE,
  locateWindowsOnDataBuffer,
  peSections,
} = locator;

function main(argv) {
  if (argv.length === 0 || argv.includes('--help') || argv.includes('-h')) {
    console.log(
      'Usage: node scripts/locate-windows-on-data-buffer.cjs ' +
      '<graph_server_shared.dll> [--prologue-bytes 16]',
    );
    process.exitCode = argv.length === 0 ? 2 : 0;
    return;
  }
  const optionIndex = argv.indexOf('--prologue-bytes');
  const prologueBytes = optionIndex === -1 ? 16 : Number(argv[optionIndex + 1]);
  const binaryPath = path.resolve(argv[0]);
  const result = locateWindowsOnDataBuffer(fs.readFileSync(binaryPath), prologueBytes);
  console.log(JSON.stringify({ path: binaryPath, ...result }, null, 2));
}

if (require.main === module) main(process.argv.slice(2));

module.exports = {
  METHOD_NAME,
  METHOD_SIGNATURE,
  locateWindowsOnDataBuffer,
  peSections,
};
