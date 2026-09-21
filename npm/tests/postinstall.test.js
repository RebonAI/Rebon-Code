"use strict";

const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");

const postinstall = require("../scripts/postinstall");

function withTempEnv(run) {
  const originalLocalAppData = process.env.LOCALAPPDATA;
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rebon-postinstall-test-"));
  process.env.LOCALAPPDATA = path.join(root, "local");
  try {
    run(root);
  } finally {
    if (originalLocalAppData === undefined) {
      delete process.env.LOCALAPPDATA;
    } else {
      process.env.LOCALAPPDATA = originalLocalAppData;
    }
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function withPackage(run) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rebon-postinstall-test-"));
  const packageRoot = path.join(root, "package");
  fs.mkdirSync(path.join(packageRoot, "bin"), { recursive: true });
  fs.writeFileSync(path.join(packageRoot, "bin", "rebon"), "rebon");
  try {
    run(packageRoot);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

function createWindowsPackage(root) {
  const packageRoot = path.join(root, "package");
  const payloadDir = path.join(packageRoot, "payload");
  fs.mkdirSync(payloadDir, { recursive: true });
  fs.writeFileSync(path.join(packageRoot, "package.json"), JSON.stringify({
    name: "@rebon/cli-win32-x64",
    version: "1.2.3",
  }));
  fs.writeFileSync(path.join(payloadDir, "rebon.exe"), "rebon");
  fs.writeFileSync(path.join(payloadDir, "rebon-boa-helper.exe"), "helper");
  return packageRoot;
}

function testEnvFlagParsing() {
  assert.strictEqual(postinstall.envFlagEnabled("1"), true);
  assert.strictEqual(postinstall.envFlagEnabled("true"), true);
  assert.strictEqual(postinstall.envFlagEnabled("YES"), true);
  assert.strictEqual(postinstall.envFlagEnabled("on"), true);
  assert.strictEqual(postinstall.envFlagEnabled("0"), false);
  assert.strictEqual(postinstall.envFlagEnabled(""), false);
  assert.strictEqual(postinstall.envFlagEnabled(undefined), false);
}

function testDefaultInstallPrintsHintOnly() {
  withPackage((packageRoot) => {
    const logs = [];
    const spawns = [];

    postinstall.runPostinstall({
      packageRoot,
      platform: "linux",
      env: {},
      log: (message) => logs.push(message),
      spawnSync: (command, args) => {
        spawns.push({ command, args });
        return { status: 0 };
      },
    });

    assert.strictEqual(spawns.length, 0);
    assert.ok(logs.some((message) => message.includes("rebon agents service install")));
    assert.ok(logs.some((message) => message.includes("REBON_INSTALL_SUPERVISOR=1")));
  });
}

function testOptInRegistersSupervisor() {
  withPackage((packageRoot) => {
    const logs = [];
    let spawned = null;

    postinstall.runPostinstall({
      packageRoot,
      platform: "linux",
      env: { REBON_INSTALL_SUPERVISOR: "1" },
      log: (message) => logs.push(message),
      spawnSync: (command, args, options) => {
        spawned = { command, args, options };
        return { status: 0 };
      },
    });

    assert.strictEqual(spawned.command, path.join(packageRoot, "bin", "rebon"));
    assert.deepStrictEqual(spawned.args, ["agents", "service", "install"]);
    assert.strictEqual(spawned.options.stdio, "inherit");
    assert.ok(logs.some((message) => message.includes("registered background supervisor scheduler")));
  });
}

function testOptInFailureIsReported() {
  withPackage((packageRoot) => {
    assert.throws(
      () => postinstall.runPostinstall({
        packageRoot,
        platform: "linux",
        env: { REBON_INSTALL_SUPERVISOR: "true" },
        log: () => {},
        spawnSync: () => ({ status: 9 }),
      }),
      /status 9/
    );
  });
}

function testWindowsOptInRegistersManagedExecutable() {
  withTempEnv((root) => {
    const packageRoot = createWindowsPackage(root);
    let spawned = null;

    postinstall.runPostinstall({
      packageRoot,
      platform: "win32",
      env: { REBON_INSTALL_SUPERVISOR: "1" },
      log: () => {},
      spawnSync: (command, args) => {
        spawned = { command, args };
        return { status: 0 };
      },
    });

    assert.ok(spawned.command.endsWith(path.join("versions", "1.2.3", "rebon.exe")));
    assert.ok(fs.existsSync(spawned.command));
    assert.ok(fs.existsSync(path.join(path.dirname(spawned.command), "rebon-boa-helper.exe")));
    assert.deepStrictEqual(spawned.args, ["agents", "service", "install"]);
  });
}

const tests = [
  testEnvFlagParsing,
  testDefaultInstallPrintsHintOnly,
  testOptInRegistersSupervisor,
  testOptInFailureIsReported,
  testWindowsOptInRegistersManagedExecutable,
];

for (const test of tests) {
  test();
}
