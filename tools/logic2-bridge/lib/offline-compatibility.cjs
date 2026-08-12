#!/usr/bin/env node
'use strict';

const fs = require('node:fs');
const path = require('node:path');
const {
  inspectGraphBinary,
  loadCompatibilityProfiles,
  platformForBinaryFormat,
} = require('./compatibility.cjs');
const {
  METHOD_NAME,
  METHOD_SIGNATURE,
  locateWindowsOnDataBuffer,
} = require('./windows-hook-locator.cjs');
const { locateMacosOnDataBuffer } = require('./macos-hook-locator.cjs');

const ANALYZER_VERSION = require('../compatibility/profiles.json').analyzerVersion;
if (!Number.isSafeInteger(ANALYZER_VERSION) || ANALYZER_VERSION < 1) {
  throw new Error('compatibility/profiles.json has an invalid analyzerVersion');
}
const LOCAL_MANIFEST_SCHEMA_VERSION = 1;
const SUPPORTED_TARGETS = new Set(['darwin/arm64', 'win32/x64', 'linux/x64']);

function exactGraphMatch(left, right) {
  return left?.identityKind === right?.identityKind &&
    String(left?.identity || '').toLowerCase() === String(right?.identity || '').toLowerCase() &&
    String(left?.sha256 || '').toLowerCase() === String(right?.sha256 || '').toLowerCase();
}

function defaultLocalManifest() {
  return {
    schemaVersion: LOCAL_MANIFEST_SCHEMA_VERSION,
    analyzerVersion: ANALYZER_VERSION,
    profiles: [],
    failures: [],
  };
}

function loadLocalManifest(manifestPath) {
  if (!manifestPath || !fs.existsSync(manifestPath)) return defaultLocalManifest();
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  if (manifest.schemaVersion !== LOCAL_MANIFEST_SCHEMA_VERSION ||
      !Array.isArray(manifest.profiles) || !Array.isArray(manifest.failures)) {
    throw new Error(`Unsupported local compatibility manifest: ${manifestPath}`);
  }
  return manifest;
}

function storeLocalManifest(manifestPath, manifest) {
  if (!manifestPath) return;
  fs.mkdirSync(path.dirname(manifestPath), { recursive: true });
  const temporary = `${manifestPath}.tmp`;
  fs.writeFileSync(temporary, `${JSON.stringify(manifest, null, 2)}\n`);
  fs.renameSync(temporary, manifestPath);
}

function bufferOccurrences(buffer, needle) {
  if (!needle.length) return [];
  const offsets = [];
  let offset = 0;
  while ((offset = buffer.indexOf(needle, offset)) !== -1) {
    offsets.push(offset);
    offset += 1;
  }
  return offsets;
}

function countAsciiOccurrences(buffer, value) {
  return bufferOccurrences(buffer, Buffer.from(value, 'ascii')).length;
}

function checkedNumber(value, label) {
  if (value > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error(`${label} exceeds JavaScript's safe integer range`);
  }
  return Number(value);
}

function machoFileOffsetToRuntimeOffset(buffer, fileOffset) {
  if (buffer.readUInt32LE(0) !== 0xfeedfacf) throw new Error('Expected a little-endian Mach-O 64 image');
  const commandCount = buffer.readUInt32LE(16);
  const commandBytes = buffer.readUInt32LE(20);
  const commandEnd = Math.min(buffer.length, 32 + commandBytes);
  const segments = [];
  let commandOffset = 32;
  for (let index = 0; index < commandCount; index += 1) {
    if (commandOffset + 8 > commandEnd) throw new Error('Mach-O load commands are truncated');
    const command = buffer.readUInt32LE(commandOffset);
    const commandSize = buffer.readUInt32LE(commandOffset + 4);
    if (commandSize < 8 || commandOffset + commandSize > commandEnd) {
      throw new Error('Mach-O load command has invalid bounds');
    }
    if (command === 0x19 && commandSize >= 72) {
      segments.push({
        vmaddr: buffer.readBigUInt64LE(commandOffset + 24),
        vmsize: buffer.readBigUInt64LE(commandOffset + 32),
        fileoff: buffer.readBigUInt64LE(commandOffset + 40),
        filesize: buffer.readBigUInt64LE(commandOffset + 48),
      });
    }
    commandOffset += commandSize;
  }
  const base = segments.find(segment => segment.fileoff === 0n && segment.filesize > 0n);
  const segment = segments.find(item => {
    const begin = checkedNumber(item.fileoff, 'Mach-O segment file offset');
    const size = checkedNumber(item.filesize, 'Mach-O segment file size');
    return fileOffset >= begin && fileOffset < begin + size;
  });
  if (!base || !segment) throw new Error('Could not map the Mach-O file offset through LC_SEGMENT_64');
  const runtime = segment.vmaddr - base.vmaddr + BigInt(fileOffset) - segment.fileoff;
  if (runtime < 0n || runtime >= base.vmsize + 0x1_0000_0000n) {
    throw new Error('Mach-O runtime offset is outside the mapped image');
  }
  return checkedNumber(runtime, 'Mach-O runtime offset');
}

function elfFileOffsetToRuntimeOffset(buffer, fileOffset) {
  if (!buffer.subarray(0, 6).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46, 2, 1]))) {
    throw new Error('Expected a little-endian ELF64 image');
  }
  const programOffset = checkedNumber(buffer.readBigUInt64LE(32), 'ELF program-header offset');
  const entrySize = buffer.readUInt16LE(54);
  const entryCount = buffer.readUInt16LE(56);
  const loads = [];
  for (let index = 0; index < entryCount; index += 1) {
    const offset = programOffset + index * entrySize;
    if (offset + 56 > buffer.length) throw new Error('ELF program headers are truncated');
    if (buffer.readUInt32LE(offset) !== 1) continue;
    loads.push({
      fileoff: buffer.readBigUInt64LE(offset + 8),
      vmaddr: buffer.readBigUInt64LE(offset + 16),
      filesize: buffer.readBigUInt64LE(offset + 32),
    });
  }
  const lowestVmaddr = loads.reduce(
    (lowest, segment) => lowest === null || segment.vmaddr < lowest ? segment.vmaddr : lowest,
    null,
  );
  if (lowestVmaddr !== 0n) {
    throw new Error('ELF first PT_LOAD virtual address is not zero; refusing an ambiguous load bias');
  }
  const segment = loads.find(item => {
    const begin = checkedNumber(item.fileoff, 'ELF segment file offset');
    const size = checkedNumber(item.filesize, 'ELF segment file size');
    return fileOffset >= begin && fileOffset < begin + size;
  });
  if (!segment) throw new Error('Could not map the ELF file offset through PT_LOAD');
  return checkedNumber(
    segment.vmaddr + BigInt(fileOffset) - segment.fileoff,
    'ELF runtime offset',
  );
}

function fileOffsetToRuntimeOffset(buffer, format, fileOffset) {
  if (format === 'mach-o') return machoFileOffsetToRuntimeOffset(buffer, fileOffset);
  if (format === 'elf') return elfFileOffsetToRuntimeOffset(buffer, fileOffset);
  throw new Error(`No conservative file-to-runtime mapping is implemented for ${format}`);
}

function locateByKnownPrologues(buffer, fingerprint, platform, architecture, profiles) {
  const methodNameOccurrences = countAsciiOccurrences(buffer, METHOD_NAME);
  const methodSignatureOccurrences = countAsciiOccurrences(buffer, METHOD_SIGNATURE);

  const candidates = new Map();
  for (const profile of profiles) {
    if (profile.platform !== platform || profile.architecture !== architecture) continue;
    const prologueHex = profile.hook?.prologueHex;
    const locatorSignatureHex = profile.hook?.locatorSignatureHex;
    if (!/^(?:[0-9a-f]{2})+$/i.test(prologueHex || '') ||
        !/^(?:[0-9a-f]{2}){32,}$/i.test(locatorSignatureHex || '')) continue;
    const locatorSignature = Buffer.from(locatorSignatureHex, 'hex');
    const occurrences = bufferOccurrences(buffer, locatorSignature);
    if (occurrences.length !== 1) continue;
    const runtimeOffset = fileOffsetToRuntimeOffset(buffer, fingerprint.format, occurrences[0]);
    const key = `${runtimeOffset}:${prologueHex.toLowerCase()}`;
    candidates.set(key, {
      onDataBufferOffset: `0x${runtimeOffset.toString(16)}`,
      prologueHex: prologueHex.toLowerCase(),
      strategy: 'known-prologue-relocation',
      evidence: {
        matchedProfileId: profile.id,
        fileOffset: `0x${occurrences[0].toString(16)}`,
        locatorSignatureBytes: locatorSignature.length,
        methodNameOccurrences,
        methodSignatureOccurrences,
      },
    });
  }
  if (candidates.size !== 1) {
    throw new Error(`Known OnDataBuffer prologues did not resolve to one candidate (${candidates.size})`);
  }
  return [...candidates.values()][0];
}

function matchKnownTrampolinePrologue(located, platform, architecture, profiles) {
  const match = profiles.find(profile =>
    profile.platform === platform && profile.architecture === architecture &&
    profile.hook?.prologueHex?.toLowerCase() === located.prologueHex.toLowerCase());
  if (!match) {
    throw new Error(
      'The located OnDataBuffer entry does not match a known trampoline-safe prologue',
    );
  }
  return match;
}

function locateCandidate(buffer, fingerprint, platform, architecture, profiles) {
  if (!SUPPORTED_TARGETS.has(`${platform}/${architecture}`)) {
    throw new Error(`Native injection is not implemented for ${platform}/${architecture}`);
  }
  if (platform === 'win32' && architecture === 'x64' && fingerprint.format === 'pe') {
    const located = locateWindowsOnDataBuffer(buffer);
    const matchedProfile = matchKnownTrampolinePrologue(
      located,
      platform,
      architecture,
      profiles,
    );
    return {
      onDataBufferOffset: located.onDataBufferOffset,
      prologueHex: located.prologueHex,
      strategy: 'pe-signature-strings-pdata-and-known-prologue',
      evidence: {
        ...located.evidence,
        matchedProfileId: matchedProfile.id,
        endOffset: located.endOffset,
        unwindOffset: located.unwindOffset,
      },
    };
  }
  if (platform === 'darwin' && architecture === 'arm64' && fingerprint.format === 'mach-o') {
    try {
      return locateMacosOnDataBuffer(buffer);
    } catch (structuralError) {
      try {
        return locateByKnownPrologues(buffer, fingerprint, platform, architecture, profiles);
      } catch (signatureError) {
        throw new Error(
          `Structural Mach-O analysis failed: ${structuralError.message}; ` +
          `known-signature fallback failed: ${signatureError.message}`,
        );
      }
    }
  }
  return locateByKnownPrologues(buffer, fingerprint, platform, architecture, profiles);
}

function localProfileId(logicVersion, platform, architecture, identity) {
  const version = String(logicVersion || 'unknown').replace(/[^a-z0-9.-]+/gi, '-');
  const identityPrefix = String(identity || 'unknown').replace(/[^a-z0-9]+/gi, '').slice(0, 12).toLowerCase();
  return `local-${version}-${platform}-${architecture}-${identityPrefix || 'unknown'}`;
}

function graphRecord(fingerprint) {
  return {
    relativePath: path.basename(fingerprint.path),
    format: fingerprint.format,
    identityKind: fingerprint.identityKind,
    identity: fingerprint.identity,
    sha256: fingerprint.sha256,
  };
}

function createCandidateProfile({ fingerprint, logicVersion, platform, architecture, located }) {
  return {
    id: localProfileId(logicVersion, platform, architecture, fingerprint.identity),
    logicVersion: logicVersion || 'unknown',
    platform,
    architecture,
    runtimeLayout: 'local-offline-analysis',
    graph: graphRecord(fingerprint),
    hook: {
      status: 'candidate',
      onDataBufferOffset: located.onDataBufferOffset,
      prologueHex: located.prologueHex,
      validation:
        `Offline analyzer v${ANALYZER_VERSION} found one OnDataBuffer candidate using ` +
        `${located.strategy}; ABI and real PXLogic capture are not verified`,
    },
    analysis: {
      analyzerVersion: ANALYZER_VERSION,
      strategy: located.strategy,
      evidence: located.evidence,
    },
  };
}

function updateLocalManifest(manifest, result) {
  manifest.analyzerVersion = ANALYZER_VERSION;
  manifest.profiles = manifest.profiles.filter(profile => !exactGraphMatch(profile.graph, result.fingerprint));
  manifest.failures = manifest.failures.filter(failure => !exactGraphMatch(failure.graph, result.fingerprint));
  if (result.profile) {
    manifest.profiles.push(result.profile);
  } else {
    manifest.failures.push({
      logicVersion: result.logicVersion || 'unknown',
      platform: result.platform,
      architecture: result.architecture,
      graph: graphRecord(result.fingerprint),
      status: 'unsupported',
      analyzerVersion: ANALYZER_VERSION,
      reason: result.reason,
    });
  }
  manifest.profiles = manifest.profiles.slice(-32);
  manifest.failures = manifest.failures.slice(-32);
}

function analyzeGraph({
  graphPath,
  logicVersion,
  platform,
  architecture,
  cachePath,
  force = false,
  profiles = loadCompatibilityProfiles(),
}) {
  const fingerprint = inspectGraphBinary(path.resolve(graphPath));
  platform ||= platformForBinaryFormat(fingerprint.format);
  architecture ||= fingerprint.architecture;
  const known = profiles.find(profile =>
    profile.platform === platform && profile.architecture === architecture &&
    exactGraphMatch(profile.graph, fingerprint));
  if (known) {
    return {
      status: 'known',
      cached: false,
      analyzerVersion: ANALYZER_VERSION,
      logicVersion,
      platform,
      architecture,
      fingerprint,
      profile: known,
      reason: `Exact built-in profile ${known.id}`,
    };
  }

  const manifest = loadLocalManifest(cachePath);
  if (!force) {
    const cachedProfile = manifest.profiles.find(profile =>
      profile.analysis?.analyzerVersion === ANALYZER_VERSION &&
      profile.platform === platform && profile.architecture === architecture &&
      exactGraphMatch(profile.graph, fingerprint));
    if (cachedProfile) {
      return {
        status: 'candidate', cached: true, analyzerVersion: ANALYZER_VERSION,
        logicVersion, platform, architecture, fingerprint, profile: cachedProfile,
        reason: cachedProfile.hook.validation,
      };
    }
    const cachedFailure = manifest.failures.find(failure =>
      failure.analyzerVersion === ANALYZER_VERSION &&
      failure.platform === platform && failure.architecture === architecture &&
      exactGraphMatch(failure.graph, fingerprint));
    if (cachedFailure) {
      return {
        status: 'unsupported', cached: true, analyzerVersion: ANALYZER_VERSION,
        logicVersion, platform, architecture, fingerprint, profile: null,
        reason: cachedFailure.reason,
      };
    }
  }

  const buffer = fs.readFileSync(fingerprint.path);
  let result;
  try {
    const located = locateCandidate(buffer, fingerprint, platform, architecture, profiles);
    const profile = createCandidateProfile({
      fingerprint, logicVersion, platform, architecture, located,
    });
    result = {
      status: 'candidate', cached: false, analyzerVersion: ANALYZER_VERSION,
      logicVersion, platform, architecture, fingerprint, profile,
      reason: profile.hook.validation,
      evidence: located.evidence,
    };
  } catch (error) {
    result = {
      status: 'unsupported', cached: false, analyzerVersion: ANALYZER_VERSION,
      logicVersion, platform, architecture, fingerprint, profile: null,
      reason: error.message,
    };
  }
  updateLocalManifest(manifest, result);
  storeLocalManifest(cachePath, manifest);
  return result;
}

function parseCliArguments(argv) {
  const result = { graphPath: null, force: false };
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (argument === '--logic-version') result.logicVersion = argv[++index];
    else if (argument === '--platform') result.platform = argv[++index];
    else if (argument === '--architecture') result.architecture = argv[++index];
    else if (argument === '--cache') result.cachePath = path.resolve(argv[++index]);
    else if (argument === '--force') result.force = true;
    else if (argument === '--help' || argument === '-h') result.help = true;
    else if (!result.graphPath) result.graphPath = path.resolve(argument);
    else throw new Error(`Unexpected argument: ${argument}`);
  }
  return result;
}

function printHelp() {
  console.log(`Usage: node lib/offline-compatibility.cjs <GraphServer binary>
  [--logic-version VERSION] [--platform PLATFORM] [--architecture ARCH]
  [--cache FILE] [--force]

The analyzer is read-only with respect to Logic.  When --cache is present, it
stores an exact-fingerprint local candidate or failure record for offline reuse.`);
}

function main(argv = process.argv.slice(2)) {
  const options = parseCliArguments(argv);
  if (options.help || !options.graphPath) {
    printHelp();
    process.exitCode = options.help ? 0 : 2;
    return;
  }
  console.log(JSON.stringify(analyzeGraph(options)));
}

module.exports = {
  ANALYZER_VERSION,
  analyzeGraph,
  bufferOccurrences,
  exactGraphMatch,
  fileOffsetToRuntimeOffset,
  loadLocalManifest,
  locateByKnownPrologues,
  matchKnownTrampolinePrologue,
  storeLocalManifest,
};

if (require.main === module) {
  try {
    main();
  } catch (error) {
    console.error(`[logic2-bridge:compatibility] ${error.message}`);
    process.exitCode = 1;
  }
}
