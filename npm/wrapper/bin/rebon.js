#!/usr/bin/env node
"use strict";

const path = require("path");
const { spawn } = require("child_process");

const PLATFORM_PACKAGES = {
  "win32-x64": "@rebon/cli-win32-x64",
  "darwin-x64": "@rebon/cli-darwin-x64",
  "darwin-arm64": "@rebon/cli-darwin-arm64",
  "linux-x64": "@rebon/cli-linux-x64",
  "linux-arm64": "@rebon/cli-linux-arm64",
};

function platformKey() {
  return `${process.platform}-${process.arch}`;
}

function fail(message) {
  console.error(`rebon: ${message}`);
  process.exit(1);
}

function resolvePlatformPackageName() {
  const key = platformKey();
  const packageName = PLATFORM_PACKAGES[key];
  if (!packageName) {
    fail(
      `unsupported platform ${key}; @rebon/cli has no bundled Rebon binary for this platform.`,
    );
  }
  return packageName;
}

function loadPlatformPackageJson(packageName) {
  try {
    return require.resolve(`${packageName}/package.json`, {
      paths: [__dirname],
    });
  } catch (error) {
    fail(
      `missing optional platform package ${packageName}. Reinstall @rebon/cli; installing with --omit=optional is a likely cause. (${error.message})`,
    );
  }
}

function resolveBinTarget(packageJsonPath, packageName) {
  let packageJson;
  try {
    packageJson = require(packageJsonPath);
  } catch (error) {
    fail(
      `could not read ${packageName} package.json. Reinstall @rebon/cli. (${error.message})`,
    );
  }

  const bin = packageJson && packageJson.bin;
  const target = typeof bin === "string" ? bin : bin && bin.rebon;
  if (!target) {
    fail(
      `${packageName} does not declare a rebon bin target. Reinstall @rebon/cli.`,
    );
  }
  return path.resolve(path.dirname(packageJsonPath), target);
}

const packageName = resolvePlatformPackageName();
const packageJsonPath = loadPlatformPackageJson(packageName);
const binTarget = resolveBinTarget(packageJsonPath, packageName);
const isJavaScript = /\.js$/i.test(binTarget);
const command = isJavaScript ? process.execPath : binTarget;
const args = isJavaScript
  ? [binTarget, ...process.argv.slice(2)]
  : process.argv.slice(2);

const child = spawn(command, args, {
  stdio: "inherit",
  windowsHide: false,
  env: { ...process.env, REBON_CODE_MODE_NODE: process.execPath },
});

child.on("error", (error) => {
  fail(
    `failed to start ${packageName} bin at ${binTarget}. Reinstall @rebon/cli. (${error.message})`,
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
