/*!
 * Copyright (c) 2017–present Tomshley LLC
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 * 
 *     https://www.apache.org/licenses/LICENSE-2.0
 * 
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

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
