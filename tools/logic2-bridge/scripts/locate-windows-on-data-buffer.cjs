#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const METHOD_NAME = 'Saleae::Graph::LogicDeviceNode::OnDataBuffer';
const METHOD_SIGNATURE =
  'void __cdecl Saleae::Graph::LogicDeviceNode::OnDataBuffer' +
  '(class DeviceId,struct Saleae::Buffer)';

function peSections(buffer) {
  if (buffer.length < 64 || buffer.toString('ascii', 0, 2) !== 'MZ') {
    throw new Error('Input is not a PE binary');
  }
  const peOffset = buffer.readUInt32LE(0x3c);
  if (peOffset + 24 > buffer.length ||
      buffer.toString('ascii', peOffset, peOffset + 4) !== 'PE\0\0') {
    throw new Error('Input has an invalid PE header');
  }
  const sectionCount = buffer.readUInt16LE(peOffset + 6);
  const optionalSize = buffer.readUInt16LE(peOffset + 20);
  const sectionOffset = peOffset + 24 + optionalSize;
  const sections = [];
  for (let index = 0; index < sectionCount; index += 1) {
    const offset = sectionOffset + index * 40;
    if (offset + 40 > buffer.length) throw new Error('PE section table is truncated');
    sections.push({
      name: buffer.subarray(offset, offset + 8).toString('ascii').replace(/\0.*$/, ''),
      virtualSize: buffer.readUInt32LE(offset + 8),
      virtualAddress: buffer.readUInt32LE(offset + 12),
      rawSize: buffer.readUInt32LE(offset + 16),
      rawOffset: buffer.readUInt32LE(offset + 20),
    });
  }
  return sections;
}

function stringRvas(buffer, section, value) {
  const needle = Buffer.from(value, 'ascii');
  const end = Math.min(buffer.length, section.rawOffset + section.rawSize);
  const results = [];
  let offset = section.rawOffset;
  while ((offset = buffer.indexOf(needle, offset)) !== -1 && offset < end) {
    if (offset + needle.length <= end) {
      results.push(section.virtualAddress + offset - section.rawOffset);
    }
    offset += 1;
  }
  return results;
}

function ripRelativeReferences(buffer, text, targetRvas) {
  const targets = new Set(targetRvas);
  const end = Math.min(buffer.length, text.rawOffset + text.rawSize);
  const results = [];
  for (let rawOffset = text.rawOffset; rawOffset + 4 <= end; rawOffset += 1) {
    const fieldRva = text.virtualAddress + rawOffset - text.rawOffset;
    const targetRva = fieldRva + 4 + buffer.readInt32LE(rawOffset);
    if (targets.has(targetRva)) results.push({ fieldRva, targetRva });
  }
  return results;
}

function runtimeFunctions(buffer, pdata, text) {
  const results = [];
  const end = Math.min(buffer.length, pdata.rawOffset + pdata.rawSize);
  const textEnd = text.virtualAddress + Math.max(text.virtualSize, text.rawSize);
  for (let offset = pdata.rawOffset; offset + 12 <= end; offset += 12) {
    const beginRva = buffer.readUInt32LE(offset);
    const endRva = buffer.readUInt32LE(offset + 4);
    const unwindRva = buffer.readUInt32LE(offset + 8);
    if (beginRva >= text.virtualAddress && beginRva < endRva && endRva <= textEnd) {
      results.push({ beginRva, endRva, unwindRva });
    }
  }
  return results;
}

function locateWindowsOnDataBuffer(buffer, prologueBytes = 16) {
  if (!Number.isInteger(prologueBytes) || prologueBytes < 12 || prologueBytes > 64) {
    throw new Error(`Invalid prologue byte count: ${prologueBytes}`);
  }
  const sections = peSections(buffer);
  const text = sections.find(section => section.name === '.text');
  const rdata = sections.find(section => section.name === '.rdata');
  const pdata = sections.find(section => section.name === '.pdata');
  if (!text || !rdata || !pdata) {
    throw new Error('PE binary must contain .text, .rdata, and .pdata sections');
  }

  const nameRvas = stringRvas(buffer, rdata, METHOD_NAME);
  const signatureRvas = stringRvas(buffer, rdata, METHOD_SIGNATURE);
  if (nameRvas.length === 0 || signatureRvas.length === 0) {
    throw new Error('LogicDeviceNode::OnDataBuffer signature strings were not found');
  }
  const references = ripRelativeReferences(
    buffer,
    text,
    [...new Set([...nameRvas, ...signatureRvas])],
  );
  const functions = runtimeFunctions(buffer, pdata, text);
  const resolved = references.map(reference => ({
    ...reference,
    function: functions.find(candidate =>
      candidate.beginRva <= reference.fieldRva && reference.fieldRva < candidate.endRva),
  })).filter(reference => reference.function);
  const candidates = [...new Set(resolved.map(reference => reference.function.beginRva))];
  if (candidates.length !== 1) {
    throw new Error(
      `OnDataBuffer references did not converge on one PE runtime function (${candidates.length})`,
    );
  }

  const beginRva = candidates[0];
  const runtimeFunction = functions.find(candidate => candidate.beginRva === beginRva);
  const rawOffset = text.rawOffset + beginRva - text.virtualAddress;
  if (rawOffset < 0 || rawOffset + prologueBytes > buffer.length) {
    throw new Error('OnDataBuffer prologue is outside the PE file');
  }
  return {
    onDataBufferOffset: `0x${beginRva.toString(16)}`,
    endOffset: `0x${runtimeFunction.endRva.toString(16)}`,
    unwindOffset: `0x${runtimeFunction.unwindRva.toString(16)}`,
    prologueHex: buffer.subarray(rawOffset, rawOffset + prologueBytes).toString('hex'),
    evidence: {
      methodNameRvas: nameRvas.map(value => `0x${value.toString(16)}`),
      methodSignatureRvas: signatureRvas.map(value => `0x${value.toString(16)}`),
      references: resolved.map(reference => ({
        fieldRva: `0x${reference.fieldRva.toString(16)}`,
        targetRva: `0x${reference.targetRva.toString(16)}`,
      })),
    },
  };
}

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
