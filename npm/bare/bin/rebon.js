#!/usr/bin/env node
"use strict";

// `rebon` is a thin wrapper around @rebon/cli: it depends on @rebon/cli and
// forwards to its launcher, so `npm install -g rebon` gives the full CLI
// (@rebon/cli pulls in the matching prebuilt platform binary as an
// optionalDependency). Args, stdio, exit code and signals are passed through.
const path = require("path");
const { spawn } = require("child_process");

function fail(message) {
  console.error(`rebon: ${message}`);
  process.exit(1);
}

let target;
try {
  const pkgJsonPath = require.resolve("@rebon/cli/package.json", {
    paths: [__dirname],
  });
  const pkg = require(pkgJsonPath);
  const bin = pkg && pkg.bin;
  const rel = typeof bin === "string" ? bin : bin && bin.rebon;
  if (!rel) {
    fail(
      "@rebon/cli does not declare a rebon bin target. Reinstall: npm install -g rebon",
    );
  }
  target = path.resolve(path.dirname(pkgJsonPath), rel);
} catch (error) {
  fail(
    `could not locate @rebon/cli. Reinstall with 'npm install -g rebon' ` +
      `(installing with --omit=optional breaks it). (${error.message})`,
  );
}

const child = spawn(process.execPath, [target, ...process.argv.slice(2)], {
  stdio: "inherit",
  windowsHide: false,
  env: { ...process.env, REBON_CODE_MODE_NODE: process.execPath },
});

child.on("error", (error) => {
  fail(
    `failed to start @rebon/cli at ${target}. Reinstall with 'npm install -g rebon'. (${error.message})`,
  );
});

child.on("exit", (code, signal) => {
  if (signal) {
    try {
      process.kill(process.pid, signal);
    } catch (_) {
      process.exit(1);
    }
    return;
  }
  process.exit(code === null ? 1 : code);
});
