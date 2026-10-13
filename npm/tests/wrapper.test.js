"use strict";

const assert = require("assert");
const fs = require("fs");
const path = require("path");
const vm = require("vm");
const EventEmitter = require("events");

const launcher = path.resolve(__dirname, "../wrapper/bin/rebon.js");
for (const [platform, arch] of [["win32", "x64"], ["linux", "x64"], ["linux", "arm64"], ["darwin", "x64"], ["darwin", "arm64"]]) {
  const wrapper = path.resolve("fixture", "node_modules", "@rebon", "cli");
  const platformRoot = path.resolve(wrapper, "..", `cli-${platform}-${arch}`);
  const manifest = path.join(platformRoot, "package.json");
  const bin = platform === "win32" ? "bin/rebon.js" : "bin/rebon";
  let spawned;
  const mockRequire = (name) => {
    if (name === "path") return path;
    if (name === "child_process") return { spawn(command, args, options) {
      spawned = { command, args, options };
      return new EventEmitter();
    } };
    if (name === manifest) return { bin: { rebon: bin } };
    throw new Error(`Unexpected require ${name}`);
  };
  mockRequire.resolve = () => manifest;
  vm.runInNewContext(fs.readFileSync(launcher, "utf8"), {
    require: mockRequire,
    __dirname: path.join(wrapper, "bin"),
    console,
    process: {
      platform, arch, execPath: process.execPath,
      argv: ["node", launcher, "--version"],
      env: { REBON_NPM_INSTALL: "inherited-untrusted-value" },
    },
  });
  const source = JSON.parse(spawned.options.env.REBON_NPM_INSTALL);
  assert.strictEqual(source.wrapper, wrapper);
  assert.strictEqual(source.platform, platformRoot);
  assert.strictEqual(source.node, process.execPath);
  assert.strictEqual(source.executable, path.resolve(platformRoot, bin));
  assert.strictEqual(spawned.options.env.REBON_CODE_MODE_NODE, process.execPath);
}
