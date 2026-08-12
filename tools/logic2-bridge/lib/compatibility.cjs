'use strict';

const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');

let rawFs = fs;
try {
  // Electron patches `fs` to expose ASAR files as directories.  `original-fs`
  // is required when the archive itself must be fingerprinted or parsed.
  rawFs = require('original-fs');
} catch {
  // Ordinary Node does not provide Electron's original-fs module.
}

const DEFAULT_PROFILES_PATH = path.join(
  __dirname,
  '..',
  'compatibility',
  'profiles.json',
);

function align4(value) {
  return (value + 3) & ~3;
}

function formatUuid(bytes) {
  const hex = Buffer.from(bytes).toString('hex').toUpperCase();
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-` +
    `${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function parseMachO(buffer) {
  if (buffer.length < 32 || buffer.readUInt32LE(0) !== 0xfeedfacf) return null;
  const cpuType = buffer.readUInt32LE(4);
  const architecture = cpuType === 0x0100000c ? 'arm64' :
    cpuType === 0x01000007 ? 'x64' : `macho-cpu-${cpuType}`;
  const commandCount = buffer.readUInt32LE(16);
  const commandBytes = buffer.readUInt32LE(20);
  let offset = 32;
  const commandsEnd = Math.min(buffer.length, offset + commandBytes);
  for (let index = 0; index < commandCount && offset + 8 <= commandsEnd; index += 1) {
    const command = buffer.readUInt32LE(offset);
    const size = buffer.readUInt32LE(offset + 4);
    if (size < 8 || offset + size > commandsEnd) break;
    if (command === 0x1b && size >= 24) {
      return {
        format: 'mach-o',
        architecture,
        identityKind: 'macho-lc-uuid',
        identity: formatUuid(buffer.subarray(offset + 8, offset + 24)),
      };
    }
    offset += size;
  }
  return { format: 'mach-o', architecture, identityKind: null, identity: null };
}

function parseElfNotes(buffer, offset, length) {
  const end = Math.min(buffer.length, offset + length);
  while (offset + 12 <= end) {
    const nameLength = buffer.readUInt32LE(offset);
    const descriptionLength = buffer.readUInt32LE(offset + 4);
    const type = buffer.readUInt32LE(offset + 8);
    const nameOffset = offset + 12;
    const descriptionOffset = nameOffset + align4(nameLength);
    const next = descriptionOffset + align4(descriptionLength);
    if (next > end) break;
    const name = buffer.subarray(nameOffset, nameOffset + nameLength).toString('ascii')
      .replace(/\0+$/, '');
    if (name === 'GNU' && type === 3) {
      return buffer.subarray(descriptionOffset, descriptionOffset + descriptionLength)
        .toString('hex');
    }
    offset = next;
  }
  return null;
}

function parseElf(buffer) {
  if (buffer.length < 64 || !buffer.subarray(0, 4).equals(Buffer.from([0x7f, 0x45, 0x4c, 0x46]))) {
    return null;
  }
  if (buffer[4] !== 2 || buffer[5] !== 1) {
    throw new Error('Only little-endian ELF64 GraphServer binaries are supported');
  }
  const machine = buffer.readUInt16LE(18);
  const architecture = machine === 62 ? 'x64' : machine === 183 ? 'arm64' : `elf-machine-${machine}`;
  const programOffset = Number(buffer.readBigUInt64LE(32));
  const programEntrySize = buffer.readUInt16LE(54);
  const programCount = buffer.readUInt16LE(56);
  for (let index = 0; index < programCount; index += 1) {
    const header = programOffset + index * programEntrySize;
    if (header + 56 > buffer.length) break;
    if (buffer.readUInt32LE(header) !== 4) continue;
    const noteOffset = Number(buffer.readBigUInt64LE(header + 8));
    const noteLength = Number(buffer.readBigUInt64LE(header + 32));
    const buildId = parseElfNotes(buffer, noteOffset, noteLength);
    if (buildId) {
      return {
        format: 'elf',
        architecture,
        identityKind: 'elf-gnu-build-id',
        identity: buildId,
      };
    }
  }
  return { format: 'elf', architecture, identityKind: null, identity: null };
}

function peRvaToOffset(buffer, sectionOffset, sectionCount, rva) {
  for (let index = 0; index < sectionCount; index += 1) {
    const offset = sectionOffset + index * 40;
    if (offset + 40 > buffer.length) break;
    const virtualSize = buffer.readUInt32LE(offset + 8);
    const virtualAddress = buffer.readUInt32LE(offset + 12);
    const rawSize = buffer.readUInt32LE(offset + 16);
    const rawOffset = buffer.readUInt32LE(offset + 20);
    const span = Math.max(virtualSize, rawSize);
    if (rva >= virtualAddress && rva < virtualAddress + span) {
      return rawOffset + rva - virtualAddress;
    }
  }
  return null;
}

function formatPeGuid(bytes) {
  const part1 = bytes.readUInt32LE(0).toString(16).padStart(8, '0');
  const part2 = bytes.readUInt16LE(4).toString(16).padStart(4, '0');
  const part3 = bytes.readUInt16LE(6).toString(16).padStart(4, '0');
  const tail = bytes.subarray(8).toString('hex');
  return `${part1}-${part2}-${part3}-${tail.slice(0, 4)}-${tail.slice(4)}`.toUpperCase();
}

function parsePe(buffer) {
  if (buffer.length < 64 || buffer.toString('ascii', 0, 2) !== 'MZ') return null;
  const peOffset = buffer.readUInt32LE(0x3c);
  if (peOffset + 24 > buffer.length || buffer.toString('ascii', peOffset, peOffset + 4) !== 'PE\0\0') {
    return null;
  }
  const machine = buffer.readUInt16LE(peOffset + 4);
  const architecture = machine === 0x8664 ? 'x64' :
    machine === 0xaa64 ? 'arm64' : `pe-machine-${machine}`;
  const sectionCount = buffer.readUInt16LE(peOffset + 6);
  const timestamp = buffer.readUInt32LE(peOffset + 8);
  const optionalSize = buffer.readUInt16LE(peOffset + 20);
  const optionalOffset = peOffset + 24;
  if (optionalOffset + optionalSize > buffer.length) {
    return { format: 'pe', architecture, identityKind: 'pe-timestamp', identity: timestamp.toString(16) };
  }
  const optionalMagic = buffer.readUInt16LE(optionalOffset);
  const directoriesOffset = optionalOffset + (optionalMagic === 0x20b ? 112 : 96);
  const debugDirectory = directoriesOffset + 6 * 8;
  const sectionOffset = optionalOffset + optionalSize;
  if (debugDirectory + 8 <= buffer.length) {
    const debugRva = buffer.readUInt32LE(debugDirectory);
    const debugSize = buffer.readUInt32LE(debugDirectory + 4);
    const debugOffset = peRvaToOffset(buffer, sectionOffset, sectionCount, debugRva);
    if (debugOffset !== null) {
      for (let offset = debugOffset; offset + 28 <= debugOffset + debugSize; offset += 28) {
        if (buffer.readUInt32LE(offset + 12) !== 2) continue;
        const dataSize = buffer.readUInt32LE(offset + 16);
        const dataOffset = buffer.readUInt32LE(offset + 24);
        if (dataSize < 24 || dataOffset + dataSize > buffer.length) continue;
        if (buffer.toString('ascii', dataOffset, dataOffset + 4) !== 'RSDS') continue;
        const guid = formatPeGuid(buffer.subarray(dataOffset + 4, dataOffset + 20));
        const age = buffer.readUInt32LE(dataOffset + 20);
        return {
          format: 'pe',
          architecture,
          identityKind: 'pe-codeview-guid-age',
          identity: `${guid}-${age}`,
          timestamp,
        };
      }
    }
  }
  return {
    format: 'pe',
    architecture,
    identityKind: 'pe-timestamp',
    identity: timestamp.toString(16).padStart(8, '0'),
    timestamp,
  };
}

function parseBinaryIdentity(buffer) {
  return parseMachO(buffer) || parseElf(buffer) || parsePe(buffer) || {
    format: 'unknown',
    architecture: 'unknown',
    identityKind: null,
    identity: null,
  };
}

function platformForBinaryFormat(format) {
  if (format === 'mach-o') return 'darwin';
  if (format === 'elf') return 'linux';
  if (format === 'pe') return 'win32';
  return null;
}

function inspectGraphBinary(graphPath) {
  const buffer = fs.readFileSync(graphPath);
  return {
    path: graphPath,
    size: buffer.length,
    sha256: crypto.createHash('sha256').update(buffer).digest('hex'),
    ...parseBinaryIdentity(buffer),
  };
}

const GRAPH_SERVER_FILENAMES = new Set([
  'libgraph_server_shared.dylib',
  'libgraph_server_shared.so',
  'libgraph_server_shared.dll',
  'graph_server_shared.dll',
]);

function installationRoots(installationPath) {
  const absolute = path.resolve(installationPath);
  let root = absolute;
  try {
    if (fs.statSync(absolute).isFile()) root = path.dirname(absolute);
  } catch {
    root = path.dirname(absolute);
  }
  const roots = [];
  let current = root;
  for (let depth = 0; depth < 4 && current; depth += 1) {
    roots.push(current);
    const parent = path.dirname(current);
    if (parent === current) break;
    current = parent;
  }
  return [...new Set(roots)];
}

function knownGraphPaths(root) {
  return [
    'Contents/Resources/macos-arm64/libgraph_server_shared.dylib',
    'Contents/Resources/macos-x64/libgraph_server_shared.dylib',
    'resources/linux-x64/libgraph_server_shared.so',
    'usr/lib/logic/resources/linux-x64/libgraph_server_shared.so',
    'resources/win32-x64/libgraph_server_shared.dll',
    'resources/windows-x64/libgraph_server_shared.dll',
    'resources/win-x64/libgraph_server_shared.dll',
    'resources/win32-x64/graph_server_shared.dll',
    'resources/windows-x64/graph_server_shared.dll',
    'resources/win-x64/graph_server_shared.dll',
  ].map(relative => path.join(root, relative));
}

function findGraphServerBinary(installationPath) {
  const roots = installationRoots(installationPath);
  for (const root of roots) {
    if (GRAPH_SERVER_FILENAMES.has(path.basename(root))) return root;
    for (const candidate of knownGraphPaths(root)) {
      if (fs.existsSync(candidate) && fs.statSync(candidate).isFile()) return candidate;
    }
  }

  // Package layouts have changed between Logic releases. Keep the fallback
  // bounded and skip large application data trees rather than scanning a home
  // directory when the caller selected an executable inside it.
  const skipDirectories = new Set([
    'Analyzers',
    'app.asar.unpacked',
    'node_modules',
    'pythonlibs',
    'locales',
  ]);
  for (const root of roots.slice(0, 1)) {
    const queue = [{ directory: root, depth: 0 }];
    const visited = new Set();
    while (queue.length > 0) {
      const current = queue.shift();
      let realPath;
      try {
        realPath = fs.realpathSync(current.directory);
      } catch {
        continue;
      }
      if (visited.has(realPath)) continue;
      visited.add(realPath);
      let entries;
      try {
        entries = fs.readdirSync(current.directory, { withFileTypes: true });
      } catch {
        continue;
      }
      for (const entry of entries) {
        const candidate = path.join(current.directory, entry.name);
        if (entry.isFile() && GRAPH_SERVER_FILENAMES.has(entry.name)) return candidate;
        if (entry.isDirectory() && current.depth < 8 && !skipDirectories.has(entry.name)) {
          queue.push({ directory: candidate, depth: current.depth + 1 });
        }
      }
    }
  }
  return null;
}

function asarEntryBuffer(asarPath, entryPath) {
  const data = rawFs.readFileSync(asarPath);
  if (data.length < 16) throw new Error(`Invalid ASAR header: ${asarPath}`);
  const headerSize = data.readUInt32LE(4);
  const jsonSize = data.readUInt32LE(12);
  const jsonStart = 16;
  const jsonEnd = jsonStart + jsonSize;
  if (jsonEnd > data.length || 8 + headerSize > data.length) {
    throw new Error(`Invalid ASAR header bounds: ${asarPath}`);
  }
  const header = JSON.parse(data.subarray(jsonStart, jsonEnd).toString('utf8'));
  let node = header;
  for (const segment of entryPath.split('/').filter(Boolean)) {
    node = node?.files?.[segment];
  }
  if (!node || node.files || node.size === undefined || node.offset === undefined) {
    return null;
  }
  const offset = Number(node.offset);
  const size = Number(node.size);
  const start = 8 + headerSize + offset;
  if (!Number.isSafeInteger(offset) || !Number.isSafeInteger(size) ||
      start < 0 || start + size > data.length) {
    throw new Error(`Invalid ASAR entry bounds: ${asarPath}:${entryPath}`);
  }
  return data.subarray(start, start + size);
}

function findAsarPaths(installationPath) {
  const paths = [];
  for (const root of installationRoots(installationPath).slice(0, 2)) {
    paths.push(
      path.join(root, 'Contents/Resources/app.asar'),
      path.join(root, 'resources/app.asar'),
      path.join(root, 'usr/lib/logic/resources/app.asar'),
      path.join(root, 'app.asar'),
    );
  }
  return [...new Set(paths)];
}

function readLogicVersionFromInstallation(installationPath) {
  for (const asarPath of findAsarPaths(installationPath)) {
    if (!fs.existsSync(asarPath)) continue;
    try {
      const packageJson = asarEntryBuffer(asarPath, 'package.json');
      const packageInfo = packageJson && JSON.parse(packageJson.toString('utf8'));
      if (typeof packageInfo?.version === 'string' && packageInfo.version.trim()) {
        return packageInfo.version.trim();
      }
    } catch {
      // Continue with another layout candidate. The fingerprint remains useful
      // even when a vendor-modified ASAR has no readable package metadata.
    }
  }
  const basename = path.basename(path.resolve(installationPath));
  const match = basename.match(/(?:logic|saleae)[-_ ]?(\d+\.\d+\.\d+)/i);
  return match ? match[1] : null;
}

function loadCompatibilityManifest(profilesPath = DEFAULT_PROFILES_PATH) {
  const manifest = JSON.parse(fs.readFileSync(profilesPath, 'utf8'));
  if (manifest.schemaVersion !== 1 || !Array.isArray(manifest.profiles)) {
    throw new Error(`Unsupported compatibility profile manifest: ${profilesPath}`);
  }
  return manifest;
}

function loadCompatibilityProfiles(profilesPath = DEFAULT_PROFILES_PATH) {
  return loadCompatibilityManifest(profilesPath).profiles;
}

function matchCompatibilityProfile({
  logicVersion,
  platform,
  architecture,
  graphPath,
  profiles = loadCompatibilityProfiles(),
}) {
  const fingerprint = inspectGraphBinary(graphPath);
  const candidates = profiles.filter(profile =>
    profile.platform === platform &&
    profile.architecture === architecture,
  );
  const profile = candidates.find(candidate =>
    fingerprint.identity !== null &&
    candidate.graph.sha256.toLowerCase() === fingerprint.sha256.toLowerCase() &&
    candidate.graph.identityKind === fingerprint.identityKind &&
    candidate.graph.identity.toLowerCase() === fingerprint.identity.toLowerCase(),
  );
  return {
    fingerprint,
    profile: profile || null,
    supported: profile?.hook.status === 'verified',
  };
}

module.exports = {
  DEFAULT_PROFILES_PATH,
  asarEntryBuffer,
  findGraphServerBinary,
  inspectGraphBinary,
  readLogicVersionFromInstallation,
  loadCompatibilityManifest,
  loadCompatibilityProfiles,
  matchCompatibilityProfile,
  parseBinaryIdentity,
  platformForBinaryFormat,
};
