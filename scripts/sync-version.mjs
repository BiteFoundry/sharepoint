#!/usr/bin/env node
// Sync the version from Cargo.toml to the root and platform package.json files.
//
// Usage:
//   node scripts/sync-version.mjs           # update files in place
//   node scripts/sync-version.mjs --check   # exit 1 if anything is out of date

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const CHECK = process.argv.includes('--check');

function readCargoVersion() {
  const cargoToml = path.join(ROOT, 'Cargo.toml');
  const text = fs.readFileSync(cargoToml, 'utf8');

  // Only read the [package] table; dependency tables may also have versions.
  const section = text.match(/^\[package\]([\s\S]*?)(?=^\[|$(?![\s\S]))/m);
  if (!section) throw new Error(`could not find [package] in ${cargoToml}`);

  const version = section[1].match(/^\s*version\s*=\s*"([^"]+)"\s*$/m);
  if (!version) throw new Error(`could not find package.version in ${cargoToml}`);

  return version[1];
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function writeJson(file, data) {
  const next = `${JSON.stringify(data, null, 2)}\n`;
  const current = fs.readFileSync(file, 'utf8');
  if (current === next) return false;
  fs.writeFileSync(file, next);
  return true;
}

function main() {
  const version = readCargoVersion();

  if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) {
    throw new Error(`Cargo.toml version "${version}" is not a valid semver version`);
  }

  const files = [path.join(ROOT, 'package.json')];

  const npmDir = path.join(ROOT, 'npm');
  if (fs.existsSync(npmDir)) {
    for (const entry of fs.readdirSync(npmDir, { withFileTypes: true })) {
      if (!entry.isDirectory()) continue;
      const manifest = path.join(npmDir, entry.name, 'package.json');
      if (fs.existsSync(manifest)) files.push(manifest);
    }
  }

  let changed = 0;
  let stale = 0;

  for (const file of files) {
    const data = readJson(file);
    const previous = data.version;

    data.version = version;

    // The root package pins every platform package to the same version.
    if (data.optionalDependencies) {
      for (const name of Object.keys(data.optionalDependencies)) {
        data.optionalDependencies[name] = version;
      }
    }

    if (previous !== version) stale += 1;

    if (CHECK) {
      const serialized = `${JSON.stringify(data, null, 2)}\n`;
      if (fs.readFileSync(file, 'utf8') !== serialized) {
        console.error(`[sync-version] out of date: ${path.relative(ROOT, file)}`);
        changed += 1;
      }
      continue;
    }

    if (writeJson(file, data)) {
      console.log(
        `[sync-version] ${path.relative(ROOT, file)}: ${previous ?? '(none)'} -> ${version}`,
      );
      changed += 1;
    }
  }

  if (CHECK) {
    if (changed > 0) {
      console.error(`[sync-version] ${changed} file(s) need updating; run node scripts/sync-version.mjs`);
      process.exitCode = 1;
    } else {
      console.log(`[sync-version] all package versions are ${version}`);
    }
    return;
  }

  if (changed === 0) {
    console.log(`[sync-version] all package versions already ${version}`);
  } else {
    console.log(`[sync-version] updated ${changed} file(s) to ${version} (${stale} version changes)`);
  }
}

try {
  main();
} catch (error) {
  console.error(`[sync-version] ${error.message}`);
  process.exit(1);
}
