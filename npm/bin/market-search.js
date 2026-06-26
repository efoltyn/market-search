#!/usr/bin/env node
'use strict';
// Launcher for `npx market-search ...` and the installed `market-search` bin.
// Ensures the prebuilt binary is present (downloads on first run if needed),
// then execs it with the user's args and inherited stdio (so MCP stdio works).
const { spawn } = require('child_process');
const { ensureBinary } = require('../install.js');

ensureBinary()
  .then((bin) => {
    const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });
    child.on('exit', (code, signal) => process.exit(signal ? 1 : code ?? 0));
    child.on('error', (e) => {
      process.stderr.write(`market-search: failed to launch binary: ${e.message}\n`);
      process.exit(1);
    });
  })
  .catch((e) => {
    process.stderr.write(`market-search: ${e.message}\n`);
    process.exit(1);
  });
