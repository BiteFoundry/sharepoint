import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath, pathToFileURL } from 'node:url';

import { isMainModule, runPostinstall } from '../scripts/postinstall.mjs';

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const POSTINSTALL_PATH = path.join(REPO_ROOT, 'scripts', 'postinstall.mjs');

function trySymlink(target, linkPath, type = 'file') {
  try {
    fs.symlinkSync(target, linkPath, type);
    return { ok: true };
  } catch (err) {
    if (process.platform === 'win32' && (err?.code === 'EPERM' || err?.code === 'EACCES')) {
      return { ok: false, reason: 'Windows symlink privilege unavailable (EPERM)' };
    }
    throw err;
  }
}

test('runPostinstall skips inside git/Cargo development repository', () => {
  const res = runPostinstall({ pkgRoot: REPO_ROOT });
  assert.deepEqual(res, { skipped: true, reason: 'dev_repo' });
});

test('isMainModule matches direct script path and rejects unrelated paths', () => {
  const metaUrl = pathToFileURL(POSTINSTALL_PATH).href;
  assert.equal(isMainModule(POSTINSTALL_PATH, metaUrl), true);
  assert.equal(isMainModule(path.join(REPO_ROOT, 'package.json'), metaUrl), false);
  assert.equal(isMainModule('', metaUrl), false);
});

test('postinstall main-module check executes when invoked via symlinked path', (t) => {
  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'sharepoint-postinstall-symlink-'));
  t.after(() => fs.rmSync(tmpDir, { recursive: true, force: true }));

  // Build an isolated non-dev package directory with a fake platform binary so we can
  // verify that invoking the script through a symlink actually triggers runPostinstall().
  const scopeDir = path.join(tmpDir, 'node_modules', '@bitefoundry');
  const pkgRoot = path.join(scopeDir, 'sharepoint');
  const platformKey = `${process.platform}-${process.arch}`;
  const platPkg = path.join(scopeDir, `sharepoint-${platformKey}`);
  const binName = process.platform === 'win32' ? 'sharepoint.exe' : 'sharepoint';

  fs.mkdirSync(path.join(pkgRoot, 'bin'), { recursive: true });
  fs.mkdirSync(path.join(pkgRoot, 'scripts'), { recursive: true });
  fs.mkdirSync(path.join(platPkg, 'bin'), { recursive: true });

  fs.writeFileSync(
    path.join(pkgRoot, 'package.json'),
    JSON.stringify({ name: '@bitefoundry/sharepoint', version: '0.0.0-test' }),
  );
  fs.writeFileSync(
    path.join(platPkg, 'package.json'),
    JSON.stringify({ name: `@bitefoundry/sharepoint-${platformKey}`, version: '0.0.0-test' }),
  );
  fs.writeFileSync(path.join(platPkg, 'bin', binName), 'SYNTHETIC-NATIVE-BIN');
  fs.writeFileSync(path.join(pkgRoot, 'bin', 'sharepoint.js'), '// fallback launcher\n');
  fs.writeFileSync(path.join(pkgRoot, 'bin', 'bitefoundry-onedrive.js'), '// fallback launcher\n');

  const stagedScript = path.join(pkgRoot, 'scripts', 'postinstall.mjs');
  fs.copyFileSync(POSTINSTALL_PATH, stagedScript);

  const symlinkScript = path.join(tmpDir, 'postinstall-symlink.mjs');
  const sym = trySymlink(stagedScript, symlinkScript, 'file');
  if (!sym.ok) {
    t.skip(sym.reason);
    return;
  }

  assert.equal(isMainModule(symlinkScript, pathToFileURL(stagedScript).href), true);

  execFileSync(process.execPath, [symlinkScript], {
    cwd: tmpDir,
    stdio: 'pipe',
  });

  if (process.platform === 'win32') {
    assert.equal(
      fs.readFileSync(path.join(pkgRoot, 'bin', 'sharepoint.exe'), 'utf8'),
      'SYNTHETIC-NATIVE-BIN',
    );
    assert.equal(
      fs.readFileSync(path.join(pkgRoot, 'bin', 'bitefoundry-onedrive.exe'), 'utf8'),
      'SYNTHETIC-NATIVE-BIN',
    );
  } else {
    assert.equal(
      fs.readFileSync(path.join(pkgRoot, 'bin', 'sharepoint.js'), 'utf8'),
      'SYNTHETIC-NATIVE-BIN',
    );
    assert.equal(
      fs.readFileSync(path.join(pkgRoot, 'bin', 'bitefoundry-onedrive.js'), 'utf8'),
      'SYNTHETIC-NATIVE-BIN',
    );
  }
});
