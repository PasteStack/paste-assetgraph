import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, '..', '..', '..');

// VERSION is the single source of the released version: the release flow bumps
// VERSION, Cargo.toml and this package.json together; the lint-rust CI check
// and this test catch a missed one.
test('package.json version matches the repository VERSION', () => {
  const expected = fs.readFileSync(path.join(ROOT, 'VERSION'), 'utf8').trim();
  const pkg = JSON.parse(
    fs.readFileSync(path.join(ROOT, 'runtime', 'node', 'package.json'), 'utf8')
  );
  assert.equal(pkg.version, expected);
});
