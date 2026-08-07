#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const CAB_HEADER_BYTES = 512;
const INVERTED_CAB_SIGNATURE = Buffer.from([0xb2, 0xac, 0xbc, 0xb9]);

function decodeHeader(buffer) {
  const decoded = Buffer.from(buffer);
  for (let index = 0; index < decoded.length; index += 1) {
    decoded[index] ^= 0xff;
  }
  return decoded;
}

function cabinetMetadata(installer, offset) {
  if (offset < 0 || offset + 36 > installer.length) return null;
  const headerLength = Math.min(CAB_HEADER_BYTES, installer.length - offset);
  const header = decodeHeader(installer.subarray(offset, offset + headerLength));
  if (header.toString('ascii', 0, 4) !== 'MSCF') return null;

  const size = header.readUInt32LE(8);
  const filesOffset = header.readUInt32LE(16);
  const versionMinor = header[24];
  const versionMajor = header[25];
  const folderCount = header.readUInt16LE(26);
  const fileCount = header.readUInt16LE(28);
  if (size < 36 || offset + size > installer.length ||
      filesOffset < 36 || filesOffset >= size ||
      versionMajor !== 1 || folderCount === 0 || fileCount === 0) {
    return null;
  }
  return {
    offset,
    size,
    filesOffset,
    version: `${versionMajor}.${versionMinor}`,
    folderCount,
    fileCount,
    header,
  };
}

function findAdvancedInstallerCabinet(installer) {
  let searchOffset = 0;
  while (searchOffset < installer.length) {
    const offset = installer.indexOf(INVERTED_CAB_SIGNATURE, searchOffset);
    if (offset === -1) break;
    const metadata = cabinetMetadata(installer, offset);
    if (metadata) return metadata;
    searchOffset = offset + 1;
  }
  throw new Error('No valid XOR-obfuscated Advanced Installer CAB was found');
}

function writeAll(fd, buffer) {
  let offset = 0;
  while (offset < buffer.length) {
    offset += fs.writeSync(fd, buffer, offset, buffer.length - offset);
  }
}

function extractAdvancedInstallerCabinet(installerPath, outputPath) {
  const installerAbsolute = path.resolve(installerPath);
  const outputAbsolute = path.resolve(outputPath);
  if (installerAbsolute === outputAbsolute) {
    throw new Error('Installer and CAB output paths must be different');
  }

  const installer = fs.readFileSync(installerAbsolute);
  const metadata = findAdvancedInstallerCabinet(installer);
  const decodedHeaderLength = Math.min(CAB_HEADER_BYTES, metadata.size);
  fs.mkdirSync(path.dirname(outputAbsolute), { recursive: true });
  const fd = fs.openSync(outputAbsolute, 'w');
  try {
    writeAll(fd, metadata.header.subarray(0, decodedHeaderLength));
    writeAll(fd, installer.subarray(
      metadata.offset + decodedHeaderLength,
      metadata.offset + metadata.size,
    ));
  } finally {
    fs.closeSync(fd);
  }

  return {
    installerPath: installerAbsolute,
    outputPath: outputAbsolute,
    offset: metadata.offset,
    size: metadata.size,
    filesOffset: metadata.filesOffset,
    version: metadata.version,
    folderCount: metadata.folderCount,
    fileCount: metadata.fileCount,
  };
}

function main(argv) {
  if (argv.length !== 2) {
    console.error(
      'Usage: node scripts/extract-advanced-installer-cab.cjs ' +
      '<Logic installer.exe> <output.cab>',
    );
    process.exitCode = 1;
    return;
  }
  console.log(JSON.stringify(extractAdvancedInstallerCabinet(argv[0], argv[1]), null, 2));
}

if (require.main === module) main(process.argv.slice(2));

module.exports = {
  CAB_HEADER_BYTES,
  extractAdvancedInstallerCabinet,
  findAdvancedInstallerCabinet,
};
