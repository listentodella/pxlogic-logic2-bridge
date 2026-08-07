#!/usr/bin/env node
'use strict';

const path = require('node:path');
const { inspectGraphBinary } = require('../lib/compatibility.cjs');

const target = process.argv[2];
if (!target || target === '--help' || target === '-h') {
  console.log('Usage: node scripts/fingerprint-binary.cjs <GraphServer binary>');
  process.exit(target ? 0 : 2);
}

console.log(JSON.stringify(inspectGraphBinary(path.resolve(target)), null, 2));
