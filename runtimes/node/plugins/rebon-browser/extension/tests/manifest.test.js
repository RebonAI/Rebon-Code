const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");

const root = path.resolve(__dirname, "..");
const manifest = JSON.parse(fs.readFileSync(path.join(root, "manifest.json"), "utf8"));

test("manifest is MV3, local-only, and references existing entrypoints", () => {
  assert.equal(manifest.manifest_version, 3);
  assert.equal(manifest.minimum_chrome_version, "116");
  assert.ok(manifest.permissions.includes("debugger"));
  assert.ok(manifest.permissions.includes("tabGroups"));
  assert.ok(manifest.permissions.includes("alarms"));
  assert.ok(!manifest.permissions.includes("offscreen"));
  assert.deepEqual(manifest.optional_host_permissions, ["http://*/*", "https://*/*"]);
  assert.ok(fs.existsSync(path.join(root, manifest.background.service_worker)));
  assert.ok(fs.existsSync(path.join(root, manifest.action.default_popup)));
  for (const file of Object.values(manifest.icons)) {
    assert.ok(fs.existsSync(path.join(root, file)), `missing icon ${file}`);
  }
  assert.deepEqual(manifest.action.default_icon, manifest.icons);
  // Loopback-only, any port: `browser-mcp --port` moves the host and
  // the storage override follows it, so the CSP must not pin 17373.
  assert.match(
    manifest.content_security_policy.extension_pages,
    /connect-src ws:\/\/127\.0\.0\.1:\*/,
  );
});

test("extension pages use packaged scripts and no inline executable code", () => {
  const popup = fs.readFileSync(path.join(root, "popup.html"), "utf8");
  assert.match(popup, /<script src="shared\.js"><\/script>/);
  assert.match(popup, /<script src="popup\.js"><\/script>/);
  assert.doesNotMatch(popup, /<script(?! src=)[^>]*>/);
  assert.doesNotMatch(popup, /on(click|load|error)=/i);
});

test("plugin package references the installed extension directory", () => {
  const plugin = JSON.parse(fs.readFileSync(path.resolve(root, "..", "rebon-plugin.json"), "utf8"));
  const server = plugin.capabilities.mcpServers.browser;
  // The server is its own executable beside `rebon`, resolved by the
  // `{rebon_bin:…}` placeholder rather than spawned as a subcommand
  assert.equal(server.command, "{rebon_bin:rebon-browser-mcp}");
  assert.deepEqual(server.args, ["--extension-dir", "{plugin_dir}/extension"]);
});
