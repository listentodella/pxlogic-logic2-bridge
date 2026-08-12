'use strict';

const METHOD_LABEL = 'OnDataBuffer';
const SOURCE_BASENAME = 'logic_device_node.cpp';
const MACHO_MAGIC_64 = 0xfeedfacf;
const LC_SEGMENT_64 = 0x19;
const LC_FUNCTION_STARTS = 0x26;
const ARM64_HOOK_BYTES = 16;

function unsignedMasked(value, mask) {
  return (value & mask) >>> 0;
}

function checkedNumber(value, label) {
  if (value < 0n || value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`${label} exceeds JavaScript's safe integer range`);
  }
  return Number(value);
}

function fixedString(buffer, offset, length) {
  return buffer.subarray(offset, offset + length).toString('ascii').replace(/\0.*$/, '');
}

function parseMacho64(buffer) {
  if (buffer.length < 32 || buffer.readUInt32LE(0) !== MACHO_MAGIC_64) {
    throw new Error('Expected a little-endian Mach-O 64 image');
  }
  const commandCount = buffer.readUInt32LE(16);
  const commandBytes = buffer.readUInt32LE(20);
  const commandEnd = Math.min(buffer.length, 32 + commandBytes);
  const segments = [];
  let functionStarts = null;
  let commandOffset = 32;
  for (let index = 0; index < commandCount; index += 1) {
    if (commandOffset + 8 > commandEnd) throw new Error('Mach-O load commands are truncated');
    const command = buffer.readUInt32LE(commandOffset);
    const commandSize = buffer.readUInt32LE(commandOffset + 4);
    if (commandSize < 8 || commandOffset + commandSize > commandEnd) {
      throw new Error('Mach-O load command has invalid bounds');
    }
    if (command === LC_SEGMENT_64 && commandSize >= 72) {
      const sectionCount = buffer.readUInt32LE(commandOffset + 64);
      if (72 + sectionCount * 80 > commandSize) {
        throw new Error('Mach-O section table exceeds its segment command');
      }
      const segment = {
        name: fixedString(buffer, commandOffset + 8, 16),
        vmaddr: buffer.readBigUInt64LE(commandOffset + 24),
        vmsize: buffer.readBigUInt64LE(commandOffset + 32),
        fileoff: buffer.readBigUInt64LE(commandOffset + 40),
        filesize: buffer.readBigUInt64LE(commandOffset + 48),
        sections: [],
      };
      for (let sectionIndex = 0; sectionIndex < sectionCount; sectionIndex += 1) {
        const sectionOffset = commandOffset + 72 + sectionIndex * 80;
        segment.sections.push({
          name: fixedString(buffer, sectionOffset, 16),
          segment: fixedString(buffer, sectionOffset + 16, 16),
          address: buffer.readBigUInt64LE(sectionOffset + 32),
          size: buffer.readBigUInt64LE(sectionOffset + 40),
          fileOffset: BigInt(buffer.readUInt32LE(sectionOffset + 48)),
        });
      }
      segments.push(segment);
    } else if (command === LC_FUNCTION_STARTS && commandSize >= 16) {
      functionStarts = {
        fileOffset: buffer.readUInt32LE(commandOffset + 8),
        size: buffer.readUInt32LE(commandOffset + 12),
      };
    }
    commandOffset += commandSize;
  }
  const image = segments.find(segment => segment.fileoff === 0n && segment.filesize > 0n);
  const text = segments.flatMap(segment => segment.sections)
    .find(section => section.segment === '__TEXT' && section.name === '__text');
  const cstring = segments.flatMap(segment => segment.sections)
    .find(section => section.segment === '__TEXT' && section.name === '__cstring');
  if (!image || !text || !cstring || !functionStarts) {
    throw new Error('Mach-O requires __TEXT,__text, __TEXT,__cstring, and LC_FUNCTION_STARTS');
  }
  if (functionStarts.fileOffset + functionStarts.size > buffer.length) {
    throw new Error('LC_FUNCTION_STARTS data exceeds the Mach-O file');
  }
  return { segments, image, text, cstring, functionStarts };
}

function runtimeToFileOffset(macho, runtimeAddress) {
  const absoluteAddress = macho.image.vmaddr + BigInt(runtimeAddress);
  const segment = macho.segments.find(item =>
    absoluteAddress >= item.vmaddr && absoluteAddress < item.vmaddr + item.filesize);
  if (!segment) throw new Error(`Mach-O address 0x${runtimeAddress.toString(16)} is not file-backed`);
  return checkedNumber(
    segment.fileoff + absoluteAddress - segment.vmaddr,
    'Mach-O file offset',
  );
}

function sectionRuntimeRange(macho, section) {
  return {
    start: checkedNumber(section.address - macho.image.vmaddr, 'section runtime address'),
    size: checkedNumber(section.size, 'section size'),
    fileOffset: checkedNumber(section.fileOffset, 'section file offset'),
  };
}

function decodeUleb128(buffer, offset, end) {
  let value = 0n;
  let shift = 0n;
  while (offset < end && shift <= 63n) {
    const byte = buffer[offset++];
    value |= BigInt(byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) return { value, offset };
    shift += 7n;
  }
  throw new Error('LC_FUNCTION_STARTS contains an invalid ULEB128 value');
}

function parseFunctionStarts(buffer, macho) {
  const end = macho.functionStarts.fileOffset + macho.functionStarts.size;
  let offset = macho.functionStarts.fileOffset;
  let address = macho.image.vmaddr;
  const starts = [];
  while (offset < end) {
    const decoded = decodeUleb128(buffer, offset, end);
    offset = decoded.offset;
    if (decoded.value === 0n) break;
    address += decoded.value;
    starts.push(checkedNumber(address - macho.image.vmaddr, 'function runtime address'));
  }
  if (starts.length === 0) throw new Error('LC_FUNCTION_STARTS did not contain any functions');
  return starts;
}

function functionForAddress(starts, address) {
  let low = 0;
  let high = starts.length - 1;
  let result = -1;
  while (low <= high) {
    const middle = Math.floor((low + high) / 2);
    if (starts[middle] <= address) {
      result = middle;
      low = middle + 1;
    } else {
      high = middle - 1;
    }
  }
  if (result < 0) return null;
  return {
    start: starts[result],
    end: starts[result + 1] || null,
  };
}

function stringAddresses(buffer, macho, value, exact) {
  const section = sectionRuntimeRange(macho, macho.cstring);
  const needle = Buffer.from(value, 'ascii');
  const end = section.fileOffset + section.size;
  const addresses = new Set();
  let offset = section.fileOffset;
  while ((offset = buffer.indexOf(needle, offset)) !== -1 && offset < end) {
    const after = offset + needle.length;
    if (after <= end && exact &&
        (offset === section.fileOffset || buffer[offset - 1] === 0) && buffer[after] === 0) {
      addresses.add(section.start + offset - section.fileOffset);
    } else if (after <= end && !exact) {
      let stringStart = offset;
      while (stringStart > section.fileOffset && buffer[stringStart - 1] !== 0) stringStart -= 1;
      addresses.add(section.start + stringStart - section.fileOffset);
    }
    offset += 1;
  }
  return [...addresses];
}

function decodeAdrpTarget(word, instructionAddress) {
  if (unsignedMasked(word, 0x9f000000) !== 0x90000000) return null;
  const immediateLow = (word >>> 29) & 0x3;
  const immediateHigh = (word >>> 5) & 0x7ffff;
  let immediate = immediateHigh * 4 + immediateLow;
  if (immediate >= 0x100000) immediate -= 0x200000;
  return Math.floor(instructionAddress / 0x1000) * 0x1000 + immediate * 0x1000;
}

function decodeAddImmediate(word, expectedRegister) {
  if (unsignedMasked(word, 0xff000000) !== 0x91000000 ||
      ((word >>> 5) & 0x1f) !== expectedRegister) return null;
  const immediate = (word >>> 10) & 0xfff;
  return {
    destinationRegister: word & 0x1f,
    immediate: immediate * (((word >>> 22) & 1) ? 0x1000 : 1),
  };
}

function arm64StringReferences(buffer, macho, targetAddresses) {
  const targets = new Set(targetAddresses);
  const text = sectionRuntimeRange(macho, macho.text);
  const references = [];
  for (let relative = 0; relative + 8 <= text.size; relative += 4) {
    const fileOffset = text.fileOffset + relative;
    const instructionAddress = text.start + relative;
    const adrp = buffer.readUInt32LE(fileOffset);
    const page = decodeAdrpTarget(adrp, instructionAddress);
    if (page === null) continue;
    const register = adrp & 0x1f;
    const add = decodeAddImmediate(buffer.readUInt32LE(fileOffset + 4), register);
    if (!add) continue;
    const targetAddress = page + add.immediate;
    if (targets.has(targetAddress)) {
      references.push({
        instructionAddress,
        targetAddress,
        destinationRegister: add.destinationRegister,
      });
    }
  }
  return references;
}

function arm64RelocationHazard(word) {
  if (unsignedMasked(word, 0x1f000000) === 0x10000000) return 'ADR/ADRP';
  if (unsignedMasked(word, 0x7c000000) === 0x14000000) return 'B/BL';
  if (unsignedMasked(word, 0xff000010) === 0x54000000) return 'B.cond';
  if (unsignedMasked(word, 0x7e000000) === 0x34000000) return 'CBZ/CBNZ';
  if (unsignedMasked(word, 0x7e000000) === 0x36000000) return 'TBZ/TBNZ';
  if (unsignedMasked(word, 0x3b000000) === 0x18000000) return 'literal load';
  return null;
}

function inspectArm64Prologue(buffer, macho, functionStart) {
  if (functionStart % 4 !== 0) throw new Error('ARM64 function entry is not instruction-aligned');
  const fileOffset = runtimeToFileOffset(macho, functionStart);
  if (fileOffset + ARM64_HOOK_BYTES > buffer.length) {
    throw new Error('ARM64 function prologue exceeds the Mach-O file');
  }
  const instructions = [];
  for (let offset = 0; offset < ARM64_HOOK_BYTES; offset += 4) {
    const word = buffer.readUInt32LE(fileOffset + offset);
    const hazard = arm64RelocationHazard(word);
    instructions.push({ word: `0x${word.toString(16).padStart(8, '0')}`, hazard });
  }
  const hazards = instructions.filter(instruction => instruction.hazard);
  if (hazards.length > 0) {
    throw new Error(
      `OnDataBuffer entry is not trampoline-safe: ${hazards.map(item => item.hazard).join(', ')}`,
    );
  }
  return {
    prologueHex: buffer.subarray(fileOffset, fileOffset + ARM64_HOOK_BYTES).toString('hex'),
    instructions,
  };
}

function bufferArgumentMoveEvidence(buffer, macho, start, end) {
  const fileOffset = runtimeToFileOffset(macho, start);
  const byteLength = Math.min((end || start + 256) - start, 256);
  const moves = [];
  for (let offset = 0; offset + 4 <= byteLength; offset += 4) {
    const word = buffer.readUInt32LE(fileOffset + offset);
    if (unsignedMasked(word, 0xffe0ffe0) === 0xaa0003e0 && ((word >>> 16) & 0x1f) === 3) {
      moves.push({
        instructionOffset: `0x${offset.toString(16)}`,
        destinationRegister: `x${word & 0x1f}`,
      });
    }
  }
  return moves;
}

function bufferSizeLoadEvidence(buffer, macho, start, end) {
  const fileOffset = runtimeToFileOffset(macho, start);
  const byteLength = Math.min((end || start + 256) - start, 256);
  const loads = [];
  for (let offset = 0; offset + 4 <= byteLength; offset += 4) {
    const word = buffer.readUInt32LE(fileOffset + offset);
    const instructionClass = unsignedMasked(word, 0xffc00000);
    if (((word >>> 5) & 0x1f) === 3 &&
        [0xf9400000, 0xfd400000].includes(instructionClass) &&
        ((word >>> 10) & 0xfff) * 8 === 16) {
      loads.push({
        instructionOffset: `0x${offset.toString(16)}`,
        destinationRegister: word & 0x1f,
        kind: instructionClass === 0xfd400000 ? 'LDR D' : 'LDR X',
      });
    }
  }
  return loads;
}

function locateMacosOnDataBuffer(buffer) {
  const macho = parseMacho64(buffer);
  const starts = parseFunctionStarts(buffer, macho);
  const methodAddresses = stringAddresses(buffer, macho, METHOD_LABEL, true);
  const sourceAddresses = stringAddresses(buffer, macho, SOURCE_BASENAME, false);
  if (methodAddresses.length === 0 || sourceAddresses.length === 0) {
    throw new Error('Mach-O OnDataBuffer diagnostic strings were not found');
  }
  const methodReferences = arm64StringReferences(buffer, macho, methodAddresses);
  const sourceReferences = arm64StringReferences(buffer, macho, sourceAddresses);
  const sourceFunctions = new Set(sourceReferences
    .map(reference => functionForAddress(starts, reference.instructionAddress)?.start)
    .filter(value => value !== undefined));
  const candidates = new Map();
  for (const reference of methodReferences) {
    const range = functionForAddress(starts, reference.instructionAddress);
    if (!range || !sourceFunctions.has(range.start)) continue;
    candidates.set(range.start, range);
  }
  if (candidates.size !== 1) {
    throw new Error(
      `Mach-O OnDataBuffer string references did not resolve to one function (${candidates.size})`,
    );
  }
  const range = [...candidates.values()][0];
  if (range.end === null || range.end - range.start < ARM64_HOOK_BYTES) {
    throw new Error('OnDataBuffer is smaller than the ARM64 hook patch');
  }
  const prologue = inspectArm64Prologue(buffer, macho, range.start);
  const bufferArgumentMoves = bufferArgumentMoveEvidence(buffer, macho, range.start, range.end);
  const bufferSizeLoads = bufferSizeLoadEvidence(buffer, macho, range.start, range.end);
  return {
    onDataBufferOffset: `0x${range.start.toString(16)}`,
    prologueHex: prologue.prologueHex,
    strategy: 'macho-cstring-xrefs-function-starts-safe-prologue',
    evidence: {
      functionStart: `0x${range.start.toString(16)}`,
      functionEnd: range.end === null ? null : `0x${range.end.toString(16)}`,
      functionSize: range.end === null ? null : range.end - range.start,
      functionStartCount: starts.length,
      methodStringAddresses: methodAddresses.map(value => `0x${value.toString(16)}`),
      sourceStringAddresses: sourceAddresses.map(value => `0x${value.toString(16)}`),
      methodReferences: methodReferences.map(reference => ({
        instructionAddress: `0x${reference.instructionAddress.toString(16)}`,
        targetAddress: `0x${reference.targetAddress.toString(16)}`,
      })),
      sourceReferencesInCandidate: sourceReferences
        .filter(reference => functionForAddress(starts, reference.instructionAddress)?.start === range.start)
        .map(reference => ({
          instructionAddress: `0x${reference.instructionAddress.toString(16)}`,
          targetAddress: `0x${reference.targetAddress.toString(16)}`,
        })),
      prologueInstructions: prologue.instructions,
      bufferArgumentMoves,
      bufferSizeLoads,
    },
  };
}

module.exports = {
  ARM64_HOOK_BYTES,
  METHOD_LABEL,
  SOURCE_BASENAME,
  arm64RelocationHazard,
  arm64StringReferences,
  bufferArgumentMoveEvidence,
  bufferSizeLoadEvidence,
  inspectArm64Prologue,
  locateMacosOnDataBuffer,
  parseFunctionStarts,
  parseMacho64,
  runtimeToFileOffset,
};
