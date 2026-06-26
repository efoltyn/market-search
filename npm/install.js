'use strict';
// market-search npm wrapper: downloads the right prebuilt binary from the
// GitHub release matching this package version, caches it next to this file,
// and hands the path to bin/market-search.js. No Rust toolchain required.
// Pattern: esbuild / ruff / git-cliff (prebuilt binary behind an npm package).
const fs = require('fs');
const path = require('path');
const https = require('https');
const pkg = require('./package.json');

const REPO = 'efoltyn/market-search';
const VERSION = pkg.version;

// node platform-arch  ->  rust target triple of the released asset
const TARGETS = {
  'darwin-arm64': 'aarch64-apple-darwin',
  'darwin-x64': 'x86_64-apple-darwin',
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'win32-x64': 'x86_64-pc-windows-msvc',
};

function rustTarget() {
  const key = `${process.platform}-${process.arch}`;
  const t = TARGETS[key];
  if (!t) {
    throw new Error(
      `market-search: no prebuilt binary for ${key} yet. ` +
      `Build from source instead: cargo install market-search`
    );
  }
  return t;
}

function binPath() {
  const exe = process.platform === 'win32' ? 'market-search.exe' : 'market-search';
  return path.join(__dirname, 'bin', exe);
}

function download(url, dest, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 10) return reject(new Error('too many redirects'));
    https
      .get(url, { headers: { 'User-Agent': 'market-search-npm' } }, (res) => {
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume();
          return resolve(download(res.headers.location, dest, redirects + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`download failed: HTTP ${res.statusCode} for ${url}`));
        }
        const tmp = dest + '.download';
        const f = fs.createWriteStream(tmp);
        res.pipe(f);
        f.on('finish', () => f.close(() => {
          fs.renameSync(tmp, dest);
          resolve();
        }));
        f.on('error', reject);
      })
      .on('error', reject);
  });
}

async function ensureBinary() {
  const dest = binPath();
  if (fs.existsSync(dest)) return dest;
  const target = rustTarget();
  const ext = process.platform === 'win32' ? '.exe' : '';
  const asset = `market-search-${target}${ext}`;
  const url = `https://github.com/${REPO}/releases/download/v${VERSION}/${asset}`;
  fs.mkdirSync(path.dirname(dest), { recursive: true });
  process.stderr.write(`market-search: fetching prebuilt binary (${target})...\n`);
  await download(url, dest);
  if (process.platform !== 'win32') fs.chmodSync(dest, 0o755);
  return dest;
}

module.exports = { ensureBinary, binPath, rustTarget };

// Run as `node install.js` (npm postinstall). Best-effort: if it fails here,
// the launcher (bin/market-search.js) retries the download on first run.
if (require.main === module) {
  ensureBinary()
    .then(() => process.stderr.write('market-search: ready.\n'))
    .catch((e) => process.stderr.write(`market-search: ${e.message}\n`));
}
