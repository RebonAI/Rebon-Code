"use strict";

const assert = require("assert");
const EventEmitter = require("events");
const fs = require("fs");
const os = require("os");
const path = require("path");

const managed = require("../lib/windows-managed");

function withTempEnv(run) {
  const originalLocalAppData = process.env.LOCALAPPDATA;
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "rebon-npm-test-"));
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

function createPackage(root, options = {}) {
  const packageRoot = path.join(root, "package");
  const payloadDir = path.join(packageRoot, "payload");
  fs.mkdirSync(payloadDir, { recursive: true });
  fs.writeFileSync(path.join(packageRoot, "package.json"), JSON.stringify({
    name: options.name || "@rebon/cli-win32-x64",
    version: options.version || "1.2.3",
    ...(options.extraPackageJson || {}),
  }));
  fs.writeFileSync(path.join(payloadDir, "rebon.exe"), options.rebonContents || "rebon");
  fs.writeFileSync(path.join(payloadDir, "rebon-boa-helper.exe"), options.helperContents || "helper");
  fs.writeFileSync(path.join(payloadDir, "rg.exe"), options.rgContents || "rg");
  return packageRoot;
}

function assertNotTimestampedExe(filePath) {
  assert.strictEqual(path.basename(filePath), "rebon.exe");
  assert.ok(!/rebon-.+\.exe$/i.test(path.basename(filePath)), filePath);
}

function testInstallsFixedVersionLayout() {
  withTempEnv((root) => {
    const packageRoot = createPackage(root);
    const packageInfo = managed.readPackageInfo(packageRoot);
    const exe = managed.installManagedPackage(packageInfo);
    const binDir = managed.managedBinDir();
    const expectedExe = path.join(binDir, "versions", "1.2.3", "rebon.exe");

    assert.strictEqual(exe, expectedExe);
    assert.ok(fs.existsSync(expectedExe));
    assert.ok(fs.existsSync(path.join(binDir, "versions", "1.2.3", "rebon-boa-helper.exe")));
    assert.ok(fs.existsSync(path.join(binDir, "versions", "1.2.3", "rg.exe")));
    assert.ok(!fs.existsSync(path.join(binDir, "rg.exe")));
    assertNotTimestampedExe(exe);

    const manifest = managed.readCurrentManifest(binDir);
    assert.strictEqual(manifest.path, "versions/1.2.3/rebon.exe");
    assert.strictEqual(manifest.versionKey, "1.2.3");
  });
}

function testUsesBuildMetadataWhenPresent() {
  withTempEnv((root) => {
    const packageRoot = createPackage(root, {
      version: "1.2.3+meta",
      extraPackageJson: { rebon: { buildId: "abc123" } },
    });
    const packageInfo = managed.readPackageInfo(packageRoot);
    const exe = managed.installManagedPackage(packageInfo);

    assert.ok(exe.endsWith(path.join("versions", "1.2.3_meta-abc123", "rebon.exe")));
    assert.strictEqual(managed.readCurrentManifest(managed.managedBinDir()).path, "versions/1.2.3_meta-abc123/rebon.exe");
  });
}

function testMigratesLegacyManifest() {
  withTempEnv((root) => {
    const packageRoot = createPackage(root);
    const binDir = managed.managedBinDir();
    fs.mkdirSync(binDir, { recursive: true });
    const legacyExe = path.join(binDir, `rebon-1.2.3-2026-05-14T00_00_00_000Z-${process.pid}-abcdef.exe`);
    fs.writeFileSync(legacyExe, "legacy");
    fs.writeFileSync(path.join(binDir, "current.json"), JSON.stringify({
      package: "@rebon/cli-win32-x64",
      version: "1.2.3",
      path: legacyExe,
    }));

    const packageInfo = managed.readPackageInfo(packageRoot);
    const exe = managed.resolveExecutable(packageInfo).exe;
    const manifest = managed.readCurrentManifest(binDir);

    assert.strictEqual(exe, path.join(binDir, "versions", "1.2.3", "rebon.exe"));
    assert.strictEqual(manifest.path, "versions/1.2.3/rebon.exe");
    assertNotTimestampedExe(exe);
  });
}

function testCleanupKeepsCurrentVersionDirectory() {
  withTempEnv(() => {
    const binDir = managed.managedBinDir();
    fs.mkdirSync(path.join(binDir, "versions", "current"), { recursive: true });
    fs.mkdirSync(path.join(binDir, "versions", "old"), { recursive: true });
    fs.mkdirSync(path.join(binDir, "staging", "old"), { recursive: true });
    fs.writeFileSync(path.join(binDir, "versions", "current", "rebon.exe"), "current");
    fs.writeFileSync(path.join(binDir, "versions", "old", "rebon.exe"), "old");
    fs.writeFileSync(path.join(binDir, "staging", "old", "rebon.exe"), "old staging");
    fs.writeFileSync(path.join(binDir, "rebon-1.0.0-2026-05-14.exe"), "legacy");

    managed.cleanupOldVersions(binDir, "current");

    assert.ok(fs.existsSync(path.join(binDir, "versions", "current", "rebon.exe")));
    assert.ok(!fs.existsSync(path.join(binDir, "versions", "old")));
    assert.ok(!fs.existsSync(path.join(binDir, "staging", "old")));
    assert.ok(!fs.existsSync(path.join(binDir, "rebon-1.0.0-2026-05-14.exe")));
  });
}

function testFallsBackWhenCurrentVersionDirectoryCannotBeReplaced() {
  withTempEnv((root) => {
    const packageRoot = createPackage(root, { rebonContents: "old" });
    managed.installManagedPackage(managed.readPackageInfo(packageRoot));

    fs.writeFileSync(path.join(packageRoot, "payload", "rebon.exe"), "new-payload");
    const packageInfo = managed.readPackageInfo(packageRoot);
    const binDir = managed.managedBinDir();
    const lockedVersionDir = path.join(binDir, "versions", "1.2.3");
    const originalRmSync = fs.rmSync;
    fs.rmSync = (target, options) => {
      if (path.resolve(target) === path.resolve(lockedVersionDir)) {
        const error = new Error("locked");
        error.code = "EPERM";
        throw error;
      }
      return originalRmSync(target, options);
    };

    let exe;
    try {
      exe = managed.installManagedPackage(packageInfo);
    } finally {
      fs.rmSync = originalRmSync;
    }

    const manifest = managed.readCurrentManifest(binDir);
    assert.notStrictEqual(exe, path.join(lockedVersionDir, "rebon.exe"));
    assert.strictEqual(path.basename(exe), "rebon.exe");
    assert.ok(path.basename(path.dirname(exe)).startsWith("1.2.3-"));
    assert.strictEqual(manifest.path, `versions/${manifest.versionKey}/rebon.exe`);
    assert.ok(fs.existsSync(path.join(lockedVersionDir, "rebon.exe")));
  });
}

function testLauncherSpawnsManifestExecutable() {
  withTempEnv((root) => {
    const packageRoot = createPackage(root);
    const packageInfo = managed.readPackageInfo(packageRoot);
    const resolved = managed.resolveExecutable(packageInfo);
    let spawned = null;
    const fakeChild = new EventEmitter();

    managed.spawnResolvedExecutable(resolved, ["--version"], (command, args, options) => {
      spawned = { command, args, options };
      return fakeChild;
    });

    assert.strictEqual(spawned.command, path.join(managed.managedBinDir(), "versions", "1.2.3", "rebon.exe"));
    assert.deepStrictEqual(spawned.args, ["--version"]);
    assert.strictEqual(spawned.options.stdio, "inherit");
    assert.strictEqual(spawned.options.env.REBON_CODE_MODE_NODE, process.execPath);
    assert.ok(path.isAbsolute(spawned.options.env.REBON_CODE_MODE_NODE));
    assertNotTimestampedExe(spawned.command);
  });
}

const tests = [
  testInstallsFixedVersionLayout,
  testUsesBuildMetadataWhenPresent,
  testMigratesLegacyManifest,
  testCleanupKeepsCurrentVersionDirectory,
  testFallsBackWhenCurrentVersionDirectoryCannotBeReplaced,
  testLauncherSpawnsManifestExecutable,
];

for (const test of tests) {
  test();
}
