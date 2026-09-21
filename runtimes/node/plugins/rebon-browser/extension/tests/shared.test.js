const test = require("node:test");
const assert = require("node:assert/strict");

const shared = require("../shared.js");

test("URL policy permits web pages and blocks privileged schemes", () => {
  assert.equal(shared.validateBrowserUrl("about:blank"), "about:blank");
  assert.equal(shared.validateBrowserUrl("https://example.com/a"), "https://example.com/a");
  assert.equal(shared.validateBrowserUrl("http://127.0.0.1:3000"), "http://127.0.0.1:3000/");
  assert.throws(() => shared.validateBrowserUrl("file:///secret"), /Only http and https/);
  assert.throws(() => shared.validateBrowserUrl("chrome:\/\/settings"), /Only http and https/);
  assert.throws(() => shared.validateBrowserUrl("javascript:alert(1)"), /Only http and https/);
  for (const url of [
    "https://chromewebstore.google.com/detail/example/abcdefghijklmnop",
    "https://chromewebstore.google.com./detail/example/abcdefghijklmnop",
    "https://chrome.google.com/webstore/detail/example/abcdefghijklmnop",
    "https://chrome.google.com./webstore/detail/example/abcdefghijklmnop",
    "https://microsoftedge.microsoft.com/addons/detail/example/abcdefghijklmnop",
    "https://microsoftedge.microsoft.com./addons/detail/example/abcdefghijklmnop",
    "https://addons.microsoft.com/detail/example/abcdefghijklmnop",
    "https://addons.microsoft.com./detail/example/abcdefghijklmnop",
  ]) {
    assert.throws(() => shared.validateBrowserUrl(url), /extension store/);
  }
});

test("public refs preserve frame id and reject malformed values", () => {
  assert.deepEqual(shared.parsePublicRef("7|document:19"), {
    frameId: 7,
    innerRef: "document:19",
  });
  assert.throws(() => shared.parsePublicRef("document:19"), /Invalid browser element ref/);
  assert.throws(() => shared.parsePublicRef("-1|document:19"), /Invalid browser element ref/);
});

test("tab reconciliation never adopts tabs the extension did not create", () => {
  assert.deepEqual(shared.reconcileControlledTabs([10, 11], [11, 12], 10), {
    kept: [11],
    removed: [10],
    activeTabId: 11,
  });
  assert.deepEqual(shared.reconcileControlledTabs([10], [12], 10), {
    kept: [],
    removed: [10],
    activeTabId: null,
  });
});

test("password values are redacted and page text is bounded", () => {
  assert.equal(shared.safeInputValue("password", "secret"), "[redacted]");
  assert.equal(shared.safeInputValue("text", "  hello   world  "), "hello world");
  assert.equal(shared.collapseText("abcdefgh", 4), "abcd…");
});

test("modifier bits and pairing input are deterministic", () => {
  assert.equal(shared.modifierBits(["Alt", "Control", "Meta", "Shift"]), 15);
  assert.equal(shared.modifierBits(["Control"]), 2);
  assert.equal(shared.normalizePairingCode("a1 2-34567"), "123456");
});
