<table>
  <tr>
    <td width="150" align="center" valign="middle">
      <img src="../assets/app-icon/icon.png" width="128" alt="Rebon">
    </td>
    <td valign="middle">
      <h1>rebon</h1>
      <p>
        面向编码及更多场景的 agent CLI —— 一个终端 harness，通过真正的 agent
        循环驱动大模型，带工具、会话、权限和完整 TUI。
      </p>
      <p>
        <a href="https://www.npmjs.com/package/@rebon/cli"><img src="https://img.shields.io/npm/v/@rebon/cli?label=%40rebon%2Fcli" alt="npm"></a>
        <a href="../LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue" alt="Apache-2.0"></a>
        ·
        <a href="../README.md">English</a>
      </p>
    </td>
  </tr>
</table>

> 两份文档描述同一套东西；有出入时以英文版为准。

## 演示

一次完整的问答：打开斜杠命令面板，用 `@` 补全文件名，提问之后它去读文件、弹出权限
确认框；再追问一句，它还记得前面聊过什么。

![Rebon 基本用法](../assets/demo/core-loop.webp)

按 `shift+tab` 在四种权限模式之间切换：default、plan、accept edits、auto。切到 auto
之后，剩下的活它自己干完，不用再点确认。

![Rebon 权限模式](../assets/demo/modes-auto.webp)

用 `/agent` 把一件事丢给子代理，它在后台自己做，你这边可以接着问别的。做完用
`/tasks` 看它做出了什么。

![Rebon 后台子代理](../assets/demo/background-agent.webp)

用 `/rewind` 列出能退回去的位置，挑一个，代码和对话一起退回那时候。

![Rebon 回滚](../assets/demo/rewind.webp)

## 这个仓库里有什么

`rebon` 命令行 agent 本体，以及构建它所需的一切：各个 crate、插件、打包链和发布
workflow。

Rebon 的其他端各自按自己的节奏发布，**不在**这棵树里。这里的注释会提到它们，是因为
共用的那部分代码是被它们塑形的：

| 端 | 位置 |
| --- | --- |
| 桌面端 | <https://reboncode.ai> |
| 移动端 | <https://reboncode.ai> |
| Web UI（`rebon serve`） | `@rebon/rebon-web` npm 包 |

`assets/` 放的是 CLI 从这些端读取的产物 —— Windows 图标、生成的 i18n 文案表，以及
一份提交进仓库的 Web UI 构建结果；`build.rs` 会把它嵌进二进制，所以 `cargo build`
从不需要 Node。

### 关于这个仓库里的 tag

这个仓库之前是 Rebon 发布 Releases 的地方，所以 refs 里有大量来自桌面端的
`app-v*` tag。它们是刻意保留的：那些发布就在那里，删掉会让指向它们的链接失效。

它们和 CLI 无关。CLI 自己的发布是 `v*` tag，发布 workflow 也只认这个。

## 安装

```bash
npm install -g @rebon/cli
```

之后升级：

```bash
npm install -g @rebon/cli@latest
```

`@rebon/cli` 是一个很薄的启动器，通过 optional dependency 拉取匹配的原生二进制。
支持的平台：`win32-x64`、`darwin-x64`、`darwin-arm64`、`linux-x64`、`linux-arm64`。

> 如果安装时报 "missing optional platform package"，通常是你传了
> `--omit=optional` 或 `--no-optional`。去掉那个参数重装即可。

## 快速开始

```bash
# 在当前目录启动本地 TUI
rebon

# 恢复之前的会话
rebon --resume k7m2q-4xr9t-hb3wz-p8ncv

# 覆盖当前的 provider / 模型
rebon --provider openrouter --model gpt-5.5

# 作为 ACP JSON-RPC 服务跑在 stdio 上（供编辑器 / IDE 集成）
rebon --acp

# 在另一台机器上的项目里工作
rebon remote add prod deploy@build.example --path /srv/app
rebon --remote prod
```

首次运行会进入引导流程：选 provider、登录（Claude / OpenAI 走 OAuth + PKCE，或者
直接粘贴 API key），然后就可以用了。

## 命令行参数

| 参数 | 作用 |
| --- | --- |
| `--acp` | 作为 ACP JSON-RPC 服务跑在 stdio 上。 |
| `--provider <name>` | 覆盖 `~/.rebon/config.json` 里的 `activeCustomProvider`。 |
| `--model <id>` | 覆盖解析出的 provider 的默认模型。 |
| `--resume <id>` | 读取磁盘上的转录并回放进 TUI。 |
| `--remote <name>` | 通过 ssh 在已配置的远端主机上跑这个会话。 |
| `--remote-path <p>` | 该远端上的项目目录。必须与 `--remote` 一起用。 |

## 配置与数据

- `~/.rebon/config.json` —— provider、模型、凭证、默认值。
- `~/.rebon/sessions/` —— 保存的转录。
- `~/.rebon/skills/`、`~/.rebon/agents/`、`~/.rebon/memory/` —— 用户资产。
- `$REBON_LOG_DIR/rebon.log` —— TUI 日志文件（Windows 下默认
  `%TEMP%/rebon/logs/rebon.log`，其他平台 `$TMPDIR/rebon/logs/rebon.log`）。
  `--acp` 模式改为输出到 stderr。

项目级的覆盖配置放在工作目录下的 `.rebon/` 和 `.claude/` 里。

## 你会得到什么

- **本地 TUI** —— 带历史记录、粘贴、图片粘贴、`@` 提及与斜杠命令选择器、排队提交、
  模式切换的输入区；带 markdown、工具分组、思考块、计划审批、权限弹窗的流式转录；
  覆盖引导、设置、恢复、回退、快速打开、历史搜索、全局搜索、任务、后台任务、团队、
  agent 的完整对话框栈。
- **Agent 工具循环** —— Bash / PowerShell、Read / Write / Edit、Glob / Grep、Sleep、
  TaskCreate / TaskUpdate / TaskList / TaskGet / TaskStop、Agent、SkillTool、
  AskUserQuestion、SendMessage、EnterPlanMode / ExitPlanMode、ToolSearch、
  Team{Create,Delete,Files,Mailbox,Manager}、Worktree，以及 MCP 工具（stdio、
  Streamable HTTP、旧版 SSE）。
- **Provider** —— Anthropic、OpenAI 和 OpenAI-Responses，支持流式、compact /
  上下文裁剪，以及自动生成会话标题。
- **权限、钩子、沙箱** —— 对 shell、文件系统、网页抓取和 skill 调用做细粒度的
  允许 / 拒绝；用户自定义钩子；带违规上报的沙箱配置。
- **Skill、agent、记忆** —— 内置和用户自己的 skill；可派生的 worker agent 以及
  负责后台任务的协调者；带召回与呈现的持久化记忆层。
- **远端主机** —— 通过 ssh 对另一台机器上的项目跑会话。远端会装一份 server 构建，
  agent 在那边执行 —— shell、文件系统、git —— 而本机只保留界面和转录。先
  `rebon remote add`，再 `rebon --remote <name>`。
- **ACP 服务** —— 从编辑器通过 stdio JSON-RPC 驱动 Rebon：`initialize`、
  `session/new`、`session/load`、`session/list`、`session/prompt`、
  `session/cancel`、反向 RPC 的权限询问、流式工具输出。

## 常用操作

```bash
# 在当前仓库开一个新会话
rebon

# 恢复上次的会话（在 TUI 里按 Tab 可以浏览）
rebon --resume <session-id>

# 通过 ACP 接入编辑器（stdio 由编辑器管理）
rebon --acp

# 不改配置文件，临时覆盖 provider / 模型
rebon --provider anthropic --model claude-opus-4-7
```

TUI 里：

- `?` —— 键盘帮助
- `/` —— 斜杠命令选择器
- `@` —— 文件 / 符号提及
- `Tab` —— 切换面板 / 列表
- `Esc` —— 取消当前步骤或关掉对话框

## 更新

`rebon` 自带更新器，启动时会检查新版本。你也可以直接重跑：

```bash
npm install -g @rebon/cli@latest
```

## 隐私

Rebon 没有遥测 —— 没有 analytics，没有崩溃上报，没有使用事件，没有匿名安装
ID。它唯一自己发起的请求就是上面这个更新检查：一个纯 `GET`，读取
`@rebon/cli` 的公开 npm 元数据，不带任何标识，连你当前的版本都不会上报。
其余所有出站流量都是你自己发起的请求 —— 一轮模型对话、一次 `WebFetch`、
一次 MCP 调用。

[PRIVACY.zh-CN.md](PRIVACY.zh-CN.md) 列出了这个二进制所有可能发出的请求、
每条各自所在的文件，以及你自己动手核实的命令。

## 排查

- **"missing optional platform package"** —— 去掉 `--omit=optional` /
  `--no-optional` / `--ignore-optional` 重装。
- **"unsupported platform"** —— 你的 `process.platform` / `process.arch` 不在
  支持的五个目标里。
- **TUI 显示错乱** —— 确认终端支持真彩色和 Unicode 宽度表（新版 Windows Terminal、
  iTerm2、Alacritty、WezTerm、Kitty 都可以）。旧的 `cmd.exe` 和 `conhost` 不支持。
- **日志** —— 看 `$REBON_LOG_DIR/rebon.log`（TUI）或 stderr（`--acp`）。

## Friends

[Linux.Do](https://linux.do) — A new ideal community

## 许可

采用 [Apache License 2.0](../LICENSE)。

再分发时需随附许可证和 [NOTICE](../NOTICE) 里的署名声明，改动过的文件需标注已被修改。

第三方组件及其许可证列在 [THIRD_PARTY_NOTICES.txt](../THIRD_PARTY_NOTICES.txt)。
