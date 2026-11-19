#!/usr/bin/env node
// Verify that an ELF binary does not require GLIBC symbol versions newer than a ceiling (default: 2.28).
//
// Usage:
//   node scripts/verify-glibc.mjs <elf-binary-path> [--max 2.28]
//   node scripts/verify-glibc.mjs --self-test

import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

export const DEFAULT_MAX_GLIBC = '2.28';

/**
 * Parse a version string such as "2.28", "GLIBC_2.34", or "2.38.1" into a 3-part integer tuple.
 * @param {string} raw
 * @returns {[number, number, number]}
 */
export function parseVersionTuple(raw) {
  if (typeof raw !== 'string') {
    throw new Error(`Invalid GLIBC version: ${String(raw)}`);
  }
  const cleaned = raw.trim().replace(/^GLIBC_/i, '');
  const match = cleaned.match(/^(\d+)\.(\d+)(?:\.(\d+))?$/);
  if (!match) {
    throw new Error(`Invalid GLIBC version "${raw}"; expected x.y or x.y.z`);
  }
  return [
    Number.parseInt(match[1], 10),
    Number.parseInt(match[2], 10),
    match[3] !== undefined ? Number.parseInt(match[3], 10) : 0,
  ];
}

/**
 * Compare two [major, minor, patch] version tuples.
 * @param {[number, number, number]} a
 * @param {[number, number, number]} b
 * @returns {number}
 */
export function compareVersionTuples(a, b) {
  for (let i = 0; i < 3; i += 1) {
    const av = a[i] ?? 0;
    const bv = b[i] ?? 0;
    if (av !== bv) return av - bv;
  }
  return 0;
}

/**
 * Format a [major, minor, patch] tuple back to a canonical version string (omitting trailing .0 patch).
 * @param {[number, number, number]} tuple
 * @returns {string}
 */
export function formatVersionTuple(tuple) {
  const [major, minor, patch = 0] = tuple;
  return patch > 0 ? `${major}.${minor}.${patch}` : `${major}.${minor}`;
}

function normalizeTupleInput(value) {
  if (Array.isArray(value)) {
    if (value.length < 2 || value.length > 3 || value.some((n) => !Number.isInteger(n) || n < 0)) {
      throw new Error(`Invalid version tuple: ${JSON.stringify(value)}`);
    }
    return [value[0], value[1], value[2] ?? 0];
  }
  return parseVersionTuple(value);
}

/**
 * Extract unique GLIBC_* version requirements from `readelf --version-info -W` or `objdump -T` text,
 * sorted in ascending semantic version order.
 * @param {string} text
 * @returns {string[]}
 */
export function parseGlibcVersions(text) {
  if (typeof text !== 'string' || text.length === 0) {
    return [];
  }

  const seen = new Map();
  const regex = /\bGLIBC_(\d+\.\d+(?:\.\d+)?)\b/g;
  for (const match of text.matchAll(regex)) {
    const tuple = parseVersionTuple(match[1]);
    const canonical = formatVersionTuple(tuple);
    if (!seen.has(canonical)) {
      seen.set(canonical, tuple);
    }
  }

  return [...seen.entries()]
    .sort((a, b) => compareVersionTuples(a[1], b[1]))
    .map(([label]) => label);
}

/**
 * Verify that all detected GLIBC versions are <= maxTuple.
 * @param {Array<string | [number, number] | [number, number, number]>} versions
 * @param {string | [number, number] | [number, number, number]} maxTuple
 * @returns {{ ok: boolean, maxAllowed: string, maxDetected: string | null, offending: string[] }}
 */
export function checkMaxGlibc(versions, maxTuple = [2, 28, 0]) {
  const max = normalizeTupleInput(maxTuple);
  const parsed = versions
    .map((v) => {
      const tuple = normalizeTupleInput(v);
      return { label: formatVersionTuple(tuple), tuple };
    })
    .sort((a, b) => compareVersionTuples(a.tuple, b.tuple));

  const offending = [];
  const offendingSet = new Set();
  for (const item of parsed) {
    if (compareVersionTuples(item.tuple, max) > 0 && !offendingSet.has(item.label)) {
      offendingSet.add(item.label);
      offending.push(item.label);
    }
  }

  return {
    ok: offending.length === 0,
    maxAllowed: formatVersionTuple(max),
    maxDetected: parsed.length > 0 ? parsed[parsed.length - 1].label : null,
    offending,
  };
}

function isElfBinary(binaryPath) {
  const fd = fs.openSync(binaryPath, 'r');
  try {
    const header = Buffer.alloc(4);
    const bytesRead = fs.readSync(fd, header, 0, 4, 0);
    return (
      bytesRead === 4 &&
      header[0] === 0x7f &&
      header[1] === 0x45 &&
      header[2] === 0x4c &&
      header[3] === 0x46
    );
  } finally {
    fs.closeSync(fd);
  }
}

function readDynamicVersionText(binaryPath) {
  const execOpts = {
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    stdio: ['ignore', 'pipe', 'pipe'],
  };

  try {
    return {
      tool: 'readelf',
      output: execFileSync('readelf', ['--version-info', '-W', binaryPath], execOpts),
    };
  } catch (readelfErr) {
    try {
      return {
        tool: 'objdump',
        output: execFileSync('objdump', ['-T', binaryPath], execOpts),
      };
    } catch (objdumpErr) {
      throw new Error(
        `Unable to inspect ELF symbol versions for ${binaryPath}. ` +
          `Tried readelf (${readelfErr.message}) and objdump (${objdumpErr.message}). ` +
          `Install binutils on Linux to run GLIBC verification.`,
      );
    }
  }
}

export function verifyGlibcBinary(binaryPath, maxVersion = DEFAULT_MAX_GLIBC) {
  const resolved = path.resolve(binaryPath);
  if (!fs.existsSync(resolved)) {
    throw new Error(`Binary not found: ${resolved}`);
  }
  if (!isElfBinary(resolved)) {
    throw new Error(`Not an ELF binary: ${resolved}`);
  }

  const maxTuple = parseVersionTuple(maxVersion);
  const { tool, output } = readDynamicVersionText(resolved);
  const versions = parseGlibcVersions(output);
  const result = checkMaxGlibc(versions, maxTuple);

  return {
    binaryPath: resolved,
    tool,
    versions,
    ...result,
  };
}

function parseCliArgs(argv) {
  const options = {
    binaryPath: null,
    max: DEFAULT_MAX_GLIBC,
    selfTest: false,
    help: false,
  };

  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--self-test') {
      options.selfTest = true;
    } else if (arg === '--help' || arg === '-h') {
      options.help = true;
    } else if (arg === '--max') {
      const value = argv[i + 1];
      if (!value) throw new Error('--max requires a version value (e.g. --max 2.28)');
      options.max = value;
      i += 1;
    } else if (arg.startsWith('--max=')) {
      options.max = arg.slice('--max='.length);
    } else if (arg.startsWith('-')) {
      throw new Error(`Unknown option: ${arg}`);
    } else if (!options.binaryPath) {
      options.binaryPath = arg;
    } else {
      throw new Error(`Unexpected extra argument: ${arg}`);
    }
  }

  return options;
}

export function runSelfTest() {
  const readelfSample = `
Version symbols section '.gnu.version' contains 42 entries:
 Addr: 0x0000000000001234  Offset: 0x001234  Link: 5 (.dynsym)
  000:   0 (*local*)       2 (GLIBC_2.2.5)   3 (GLIBC_2.17)    4 (GLIBC_2.28)
  004:   4 (GLIBC_2.28)    5 (GLIBC_PRIVATE)

Version needs section '.gnu.version_r' contains 1 entries:
 Addr: 0x0000000000002345  Offset: 0x002345  Link: 6 (.dynstr)
  000000: Version: 1  File: libc.so.6  Cnt: 4
  0x0010:   Name: GLIBC_2.2.5  Flags: none  Version: 2
  0x0020:   Name: GLIBC_2.3.2  Flags: none  Version: 3
  0x0030:   Name: GLIBC_2.17  Flags: none  Version: 4
  0x0040:   Name: GLIBC_2.28  Flags: none  Version: 5
`;

  const parsedReadelf = parseGlibcVersions(readelfSample);
  assert.deepStrictEqual(parsedReadelf, ['2.2.5', '2.3.2', '2.17', '2.28']);

  const okCheck = checkMaxGlibc(parsedReadelf, [2, 28, 0]);
  assert.strictEqual(okCheck.ok, true);
  assert.strictEqual(okCheck.maxAllowed, '2.28');
  assert.strictEqual(okCheck.maxDetected, '2.28');
  assert.deepStrictEqual(okCheck.offending, []);

  const objdumpSample = `
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.2.5) strtol
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.33)  stat64
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.34)  __libc_start_main
0000000000000000      DF *UND*  0000000000000000 (GLIBC_2.38)  __isoc23_strtol
`;

  const parsedObjdump = parseGlibcVersions(objdumpSample);
  assert.deepStrictEqual(parsedObjdump, ['2.2.5', '2.33', '2.34', '2.38']);

  const badCheck = checkMaxGlibc(parsedObjdump, [2, 28, 0]);
  assert.strictEqual(badCheck.ok, false);
  assert.strictEqual(badCheck.maxDetected, '2.38');
  assert.deepStrictEqual(badCheck.offending, ['2.33', '2.34', '2.38']);

  const patchCheck = checkMaxGlibc(['2.28', '2.28.1', '2.29'], [2, 28, 0]);
  assert.strictEqual(patchCheck.ok, false);
  assert.deepStrictEqual(patchCheck.offending, ['2.28.1', '2.29']);

  const emptyCheck = checkMaxGlibc([], [2, 28, 0]);
  assert.strictEqual(emptyCheck.ok, true);
  assert.strictEqual(emptyCheck.maxDetected, null);
  assert.deepStrictEqual(emptyCheck.offending, []);

  assert.deepStrictEqual(parseGlibcVersions('no glibc symbols here'), []);
  assert.deepStrictEqual(parseVersionTuple('GLIBC_2.28'), [2, 28, 0]);
  assert.deepStrictEqual(parseVersionTuple('2.38.1'), [2, 38, 1]);
}

function main() {
  const options = parseCliArgs(process.argv.slice(2));

  if (options.help) {
    console.log(
      'Usage:\n' +
        '  node scripts/verify-glibc.mjs <elf-binary-path> [--max 2.28]\n' +
        '  node scripts/verify-glibc.mjs --self-test',
    );
    return;
  }

  if (options.selfTest) {
    runSelfTest();
    console.log('[verify-glibc] self-test passed');
    return;
  }

  if (!options.binaryPath) {
    throw new Error('Missing <elf-binary-path>. Pass --help for usage.');
  }

  const report = verifyGlibcBinary(options.binaryPath, options.max);
  if (report.maxDetected) {
    console.log(
      `[verify-glibc] ${options.binaryPath} (${report.tool}): highest GLIBC_${report.maxDetected} ` +
        `(max allowed: GLIBC_${report.maxAllowed}, ${report.versions.length} version(s) found)`,
    );
  } else {
    console.log(
      `[verify-glibc] ${options.binaryPath} (${report.tool}): no GLIBC_* symbol versions found ` +
        `(max allowed: GLIBC_${report.maxAllowed})`,
    );
  }

  if (!report.ok) {
    console.error(
      `[verify-glibc] ERROR: symbol version(s) exceed GLIBC_${report.maxAllowed}: ` +
        report.offending.map((v) => `GLIBC_${v}`).join(', '),
    );
    process.exitCode = 1;
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (error) {
    console.error(`[verify-glibc] ${error.message}`);
    process.exit(1);
  }
}
