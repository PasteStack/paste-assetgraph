// Fail unless the packed npm runtime carries the repository's legal files.
//
// The npm package bundles the native library, so it must ship LICENSE,
// NOTICE.md and THIRD_PARTY_LICENSES.md byte-identical to the repository
// root's. The publish job copies them in; this gate packs runtime/node and
// proves the tarball holds them. Node only, so it runs on every Node image.
//
// Usage: node scripts/check_npm_package_legal.cjs
'use strict';

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const zlib = require('node:zlib');

const root = path.resolve(__dirname, '..');
const legal = ['LICENSE', 'NOTICE.md', 'THIRD_PARTY_LICENSES.md'];

// The regular files of a gzipped ustar archive, by name.
function tarFiles(file) {
  const data = zlib.gunzipSync(fs.readFileSync(file));
  const files = new Map();
  let offset = 0;
  while (offset + 512 <= data.length) {
    const header = data.subarray(offset, offset + 512);
    if (header.every(byte => byte === 0)) break;
    const field = (start, length) =>
      header.subarray(start, start + length).toString('utf8').replace(/\0.*$/s, '');
    const name = (field(345, 155) ? field(345, 155) + '/' : '') + field(0, 100);
    const size = parseInt(field(124, 12).trim() || '0', 8);
    const type = field(156, 1);
    if (type === '0' || type === '') {
      files.set(name, data.subarray(offset + 512, offset + 512 + size));
    }
    offset += 512 + Math.ceil(size / 512) * 512;
  }
  return files;
}

const out = fs.mkdtempSync(path.join(os.tmpdir(), 'npm-legal-'));
try {
  const packed = JSON.parse(execFileSync(
    'npm', ['pack', '--json', '--pack-destination', out],
    { cwd: path.join(root, 'runtime', 'node'), encoding: 'utf8' }
  ));
  const files = tarFiles(path.join(out, packed[0].filename));
  const failures = [];
  for (const name of legal) {
    const shipped = files.get('package/' + name);
    if (!shipped) {
      failures.push(`npm package lacks ${name}`);
    } else if (!shipped.equals(fs.readFileSync(path.join(root, name)))) {
      failures.push(`npm package carries a ${name} that differs from the repository's`);
    }
  }
  if (failures.length) {
    process.stderr.write(failures.join('\n') + '\n');
    process.exitCode = 1;
  } else {
    process.stdout.write(`npm package carries ${legal.join(', ')}\n`);
  }
} finally {
  fs.rmSync(out, { recursive: true, force: true });
}
