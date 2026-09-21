const PAGE_ORIGINS = ["http://*/*", "https://*/*"];

const elements = {
  dot: document.querySelector("#status-dot"),
  statusText: document.querySelector("#status-text"),
  bridge: document.querySelector("#bridge-status"),
  access: document.querySelector("#access-status"),
  control: document.querySelector("#control-status"),
  pairingSection: document.querySelector("#pairing-section"),
  pairingCode: document.querySelector("#pairing-code"),
  pairButton: document.querySelector("#pair-button"),
  accessButton: document.querySelector("#access-button"),
  stopButton: document.querySelector("#stop-button"),
  forgetButton: document.querySelector("#forget-button"),
  message: document.querySelector("#message"),
};

let latestStatus = null;
let refreshTimer = null;

function showMessage(text, error = false) {
  if (!text) {
    elements.message.classList.add("hidden");
    return;
  }
  elements.message.textContent = text;
  elements.message.classList.toggle("error", error);
  elements.message.classList.remove("hidden");
}

function render(status) {
  latestStatus = status;
  elements.dot.className = "status-dot";
  if (status.controlling) {
    elements.dot.classList.add("controlling");
  } else if (status.authenticated) {
    elements.dot.classList.add("connected");
  }

  elements.bridge.textContent = status.authenticated
    ? "已配对"
    : status.connected
      ? "待配对"
      : "未连接";
  elements.access.textContent = status.page_access_granted ? "已授权" : "未授权";
  elements.control.textContent = status.controlling
    ? `控制中（${status.tabs.length} 个标签）`
    : "未控制";

  if (status.controlling) {
    elements.statusText.textContent = "Rebon 正在控制专用标签组";
  } else if (status.authenticated) {
    elements.statusText.textContent = "已连接，等待 Rebon 发起浏览器任务";
  } else if (status.connected) {
    elements.statusText.textContent = "输入 Rebon 显示的配对码";
  } else {
    elements.statusText.textContent = "请先启动安装了 browser plugin 的 Rebon";
  }

  elements.pairingSection.classList.toggle("hidden", !status.connected || status.authenticated);
  elements.accessButton.disabled = status.page_access_granted;
  elements.accessButton.textContent = status.page_access_granted ? "页面访问已启用" : "启用页面访问";
  elements.stopButton.disabled = !status.controlling;
  elements.forgetButton.disabled = !status.authenticated;

  if (status.transport_error) {
    showMessage(status.transport_error, true);
  } else if (!elements.message.classList.contains("error")) {
    showMessage("");
  }
}

async function refresh() {
  try {
    const response = await chrome.runtime.sendMessage({
      target: "background",
      type: "popup_status",
    });
    if (!response?.ok) {
      throw new Error(response?.error || "无法读取扩展状态。");
    }
    render(response.result);
  } catch (error) {
    showMessage(error.message || String(error), true);
  }
}

elements.pairingCode.addEventListener("input", () => {
  elements.pairingCode.value = RebonBrowserShared.normalizePairingCode(elements.pairingCode.value);
});

elements.pairButton.addEventListener("click", async () => {
  const code = elements.pairingCode.value.trim();
  if (!/^\d{6}$/.test(code)) {
    showMessage("请输入 6 位配对码。", true);
    return;
  }
  elements.pairButton.disabled = true;
  try {
    const response = await chrome.runtime.sendMessage({
      target: "background",
      type: "popup_pair",
      code,
    });
    if (!response?.ok) {
      throw new Error(response?.error || "配对失败。");
    }
    showMessage("已发送配对码，正在验证…");
    setTimeout(refresh, 250);
  } catch (error) {
    showMessage(error.message || String(error), true);
  } finally {
    elements.pairButton.disabled = false;
  }
});

elements.accessButton.addEventListener("click", async () => {
  try {
    const granted = await chrome.permissions.request({ origins: PAGE_ORIGINS });
    if (!granted) {
      throw new Error("未授予网页访问权限。");
    }
    showMessage("页面访问已启用。Rebon 仍只控制专用标签组。");
    await refresh();
  } catch (error) {
    showMessage(error.message || String(error), true);
  }
});

elements.stopButton.addEventListener("click", async () => {
  try {
    const response = await chrome.runtime.sendMessage({
      target: "background",
      type: "popup_stop",
    });
    if (!response?.ok) {
      throw new Error(response?.error || "停止控制失败。");
    }
    showMessage("Rebon 已停止控制，标签页仍保留在浏览器中。");
    await refresh();
  } catch (error) {
    showMessage(error.message || String(error), true);
  }
});

elements.forgetButton.addEventListener("click", async () => {
  try {
    const response = await chrome.runtime.sendMessage({
      target: "background",
      type: "popup_forget_pairing",
    });
    if (!response?.ok) {
      throw new Error(response?.error || "无法清除配对。");
    }
    showMessage("已忘记本机配对。下次使用需要新的配对码。");
    await refresh();
  } catch (error) {
    showMessage(error.message || String(error), true);
  }
});

refresh();
refreshTimer = setInterval(refresh, 1_000);
window.addEventListener("unload", () => clearInterval(refreshTimer));
