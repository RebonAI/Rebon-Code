# 隐私说明

**Rebon 没有遥测。** 没有任何 analytics SDK、崩溃上报、使用事件、匿名安装
ID、设备指纹或 A/B 实验服务。不存在任何一个 Rebon 服务端会收集你、你的机器、
你的提示词或你的会话。

下面列出这个二进制**所有**可能发出的网络请求，这样你可以自己核对，而不必听
我们一面之词。每条都标出了它所在的文件。

· [English](../PRIVACY.md)

## Rebon 自己会发的请求

只有一条：更新检查。

`GET https://registry.npmjs.org/@rebon%2fcli` —— 读取包的 `dist-tags`，在本地
比较版本
（[`crates/plugins/updater/src/check.rs`](../crates/plugins/updater/src/check.rs)）。

- **不带任何标识。** 裸 HTTP 客户端，没有 `User-Agent`，没有查询参数，没有我们
  自己加的任何头。**不会上报你当前的版本**——比较是在响应回来之后本地做的。
- **懒启动。** 只有当「能画出更新提示的前端」来轮询时才会发起，所以
  `rebon exec`、ACP 会话和后台 worker 根本不会发这个请求
  （[`crates/plugins/updater/src/seat.rs`](../crates/plugins/updater/src/seat.rs)）。
- **开发构建（版本 `0.0.1`）直接跳过。**
- **关掉的方式：** `~/.rebon/config.json` 里的 `update.disabled`；
  `plugins.updater.enabled = false`（整个插件不会加载，连 seat 都不存在，
  `/update` 命令也不会出现）；或者用 `REBON_UPDATE_PACKAGE` 指向你自己的包。

npm 那边能知道的：你的 IP，以及有人请求了 `@rebon/cli` 的公开元数据。这就是
非你主动发起的全部足迹。

没有启动 ping，没有心跳，没有后台同步，也没有「首次运行」注册。

## 你让它发它才发的请求

下面每一条都由你、或由你批准的一次工具调用触发。这些都不会回报给 Rebon。

| 触发 | 目标 | 携带内容 |
| --- | --- | --- |
| 任何一轮模型对话 | 你配置的 provider base URL | 你的提示词、文件和工具输出——这就是产品本身 |
| 登录 / 刷新令牌 | provider 的认证域名（如 `auth.openai.com`） | 仅 OAuth 凭据 |
| `/model refresh` | `https://reboncode.ai/api/models` | 什么都不带——纯 `GET`，无请求体。可用 `REBON_MODEL_TABLE_URL` 指向自建镜像 |
| `WebFetch` 工具 | agent 要抓取的那个 URL | 就是对该 URL 的一次普通抓取 |
| `WebSearch` 工具（回退路径） | `api.search.brave.com`、`html.duckduckgo.com` 或 `www.bing.com` | 搜索词。仅在当前 provider 没有服务端搜索能力时启用 |
| MCP 工具 | 你自己配置的 MCP server | 取决于该 server 的协议 |
| Hooks | 你的 hook 指向的地方 | 你的 hook 自己发的内容 |
| `rebon node install` | `https://nodejs.org/dist` | 什么都不带；归档有 SHA-256 校验，也可以传入镜像地址 |
| `rebon remote` | 先从 `registry.npmjs.org` 取服务端 tarball，然后 ssh 到你指定的机器 | 会话跑在你指名的那台远端机器上 |
| 图像生成 | provider 的图像接口 | 你的提示词 |

这里面唯一碰到 Rebon 自有服务的是模型表（`/model refresh`），它必须手动执行，
而且唯一调用点就是 `/model` 命令
（[`crates/rebon-session-runtime/src/commands/model.rs`](../crates/rebon-session-runtime/src/commands/model.rs)）。
Rebon 从不自动刷新它；启动时读的是本地缓存，或者编译进二进制的那份快照。

## 边角情况

- **唯一一个看起来像遥测的请求头。** 内嵌 dsh 运行时发往 DeepSeek 的请求会带
  `x-deepseek-harness-user-id`。上游包会生成一个随机 ID 并持久化下来用于遥测
  关联；Rebon 的 shim 把它替换成了固定字符串 `rebon-embedded`，它标识的是
  harness，不包含任何关于你的信息
  （[`runtimes/node/compose-runtime/payload/compose/shims/dsh-anonymous-user-id.js`](../runtimes/node/compose-runtime/payload/compose/shims/dsh-anonymous-user-id.js)）。
  它搭在你本来就要发的 `chat/completions` 上，不是一次额外请求。
- **`npm install` 自己不联网。** postinstall 脚本只负责在 npm 已经下好的原生
  二进制里挑出对的那个。
- **Web UI（`rebon serve`）完全自托管。** 没有 CDN，没有外部字体，没有第三方
  脚本，页面还设了 `<meta name="referrer" content="no-referrer">`。
- **远程控制没有内置服务端地址。** `apiBaseUrl` 要你自己配；不配就什么都不会
  连。
- **浏览器扩展**有自己的声明——无 analytics、无遥测、无广告
  （[`runtimes/node/plugins/rebon-browser/PRIVACY.md`](../runtimes/node/plugins/rebon-browser/PRIVACY.md)）。

## 你的数据在哪

全部留在你自己的磁盘上，未加密，你随时可读：

- `~/.rebon/config.json` —— provider、模型、默认值
- `~/.rebon/.credentials.json` —— API key 和 OAuth 令牌
- `~/.rebon/sessions/` —— 完整的会话记录
- `~/.rebon/skills/`、`~/.rebon/agents/`、`~/.rebon/memory/` —— 你自己的资产
- `$REBON_LOG_DIR/rebon.log` —— TUI 日志

这些 Rebon 都不会上传、同步或备份到任何地方。删掉这个目录，就是彻底卸载你的
数据。

## 自己动手核实

源码就在这里，别信清单，直接审：

```bash
# 所有可能发起 HTTP 请求的模块
grep -rln reqwest --include=*.rs crates services

# 所有硬编码的外部主机
grep -rhoE 'https?://[a-zA-Z0-9.-]+' --include=*.rs crates services | sort -u

# 依赖树里没有任何遥测 SDK（这条命令应当没有输出）
grep -niE 'sentry|statsig|opentelemetry|posthog|amplitude|mixpanel|datadog' Cargo.lock
```

或者把 `plugins.updater.enabled = false` 打开，用抓包工具跑一次 `rebon`，确认
在你发出第一条提示词之前，这个进程一个请求都不会发。

如果你发现了本文没有描述的出站请求，请提 issue —— 那是 bug，不是 feature。
