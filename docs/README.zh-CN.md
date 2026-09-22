<table>
  <tr>
    <td width="150" align="center" valign="middle">
      <img src="../assets/app-icon/icon.png" width="128" alt="Rebon">
    </td>
    <td valign="middle">
      <h1>rebon</h1>
      <p>
        一个在终端里跑的编码 agent。它真的会动手：调工具、记住上下文、按你给的
        权限行事，界面是完整的 TUI。
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

> 这份和英文版讲的是同一个东西。两边对不上的时候，以英文版为准。

## 演示

输入 `/` 会弹出命令列表，输入 `@` 能补全文件名。问它某个文件是干什么的，它要读这个
文件，会先停下来问你准不准；批准之后再追问一句，它还记得刚才说过什么。

![Rebon 基本用法](../assets/demo/core-loop.webp)

按一次 `shift+tab` 换一种权限模式，一共四种：default、plan、accept edits、auto。
换到 auto 之后它不再问你，剩下的自己做完。

![Rebon 权限模式](../assets/demo/modes-auto.webp)

用 `/agent` 另派一个 agent 去干一件事。它在后台跑，你这边不用等，可以接着问别的。
`/tasks` 里能看到它跑了多久、花了多少 token、最后交出什么。

![Rebon 后台 agent](../assets/demo/background-agent.webp)

改坏了想反悔，用 `/rewind`。它会列出你之前发过的每一条消息，选中哪一条，文件和聊天
记录就都回到你发那条消息之前的样子。

![Rebon 回退改动](../assets/demo/rewind.webp)

## 这个仓库里有什么

`rebon` 这个命令行 agent，以及把它构建出来所需要的全部东西：各个 crate、插件、
打包脚本和发布 workflow。

Rebon 还有别的客户端，各自按自己的节奏发版，**不在**这个仓库里。代码注释里会提到
它们，因为有些共用的代码当初就是为它们写的：

| 客户端 | 在哪 |
| --- | --- |
| 桌面端 | <https://reboncode.ai> |
| 移动端 | <https://reboncode.ai> |
| Web UI（`rebon serve`） | `@rebon/rebon-web` npm 包 |

`assets/` 放的是 CLI 要用、但来自这些客户端的文件：Windows 图标、生成好的 i18n
文案表，还有一份直接提交进仓库的 Web UI 构建结果。`build.rs` 会把它打进二进制，
所以 `cargo build` 用不着 Node。

### 关于这个仓库里的 tag

这个仓库以前是 Rebon 发 Release 的地方，所以留着一大堆桌面端的 `app-v*` tag。
这些是故意不删的：那些发布就挂在上面，删了指过去的链接会全部失效。

它们和 CLI 无关。CLI 自己的发布用 `v*` tag，发布 workflow 也只认这个。

## 安装

```bash
npm install -g @rebon/cli
```

之后升级：

```bash
npm install -g @rebon/cli@latest
```

`@rebon/cli` 本身只是个壳，真正的原生二进制靠 optional dependency 按平台装。
支持 `win32-x64`、`darwin-x64`、`darwin-arm64`、`linux-x64`、`linux-arm64`。

> 装的时候报 "missing optional platform package"，多半是你加了
> `--omit=optional` 或 `--no-optional`。去掉重装就行。

## 快速开始

```bash
# 在当前目录启动本地 TUI
rebon

# 接着上次的会话继续
rebon --resume k7m2q-4xr9t-hb3wz-p8ncv

# 临时换一个 provider / 模型
rebon --provider openrouter --model gpt-5.5

# 作为 ACP JSON-RPC 服务跑在 stdio 上（给编辑器 / IDE 用）
rebon --acp

# 在另一台机器上的项目里干活
rebon remote add prod deploy@build.example --path /srv/app
rebon --remote prod
```

第一次运行会带你走一遍设置：挑 provider、登录（Claude / OpenAI 走 OAuth + PKCE，
也可以直接粘 API key），然后就能用了。

## 命令行参数

| 参数 | 作用 |
| --- | --- |
| `--acp` | 作为 ACP JSON-RPC 服务跑在 stdio 上。 |
| `--provider <name>` | 盖掉 `~/.rebon/config.json` 里的 `activeCustomProvider`。 |
| `--model <id>` | 盖掉当前 provider 的默认模型。 |
| `--resume <id>` | 把存在磁盘上的聊天记录读出来，重新放回 TUI。 |
| `--remote <name>` | 通过 ssh 把这个会话跑在配置好的远端机器上。 |
| `--remote-path <p>` | 远端上的项目目录。必须和 `--remote` 一起用。 |

## 配置与数据

- `~/.rebon/config.json`：provider、模型、凭证、各种默认值。
- `~/.rebon/sessions/`：存下来的聊天记录。
- `~/.rebon/skills/`、`~/.rebon/agents/`、`~/.rebon/memory/`：你自己的东西。
- `$REBON_LOG_DIR/rebon.log`：TUI 的日志（Windows 下默认在
  `%TEMP%/rebon/logs/rebon.log`，其他平台在 `$TMPDIR/rebon/logs/rebon.log`）。
  `--acp` 模式下改成输出到 stderr。

只想对某个项目生效的配置，放在那个项目目录下的 `.rebon/` 和 `.claude/` 里。

## 它能做什么

- **本地 TUI**。输入框记得历史、能粘文字也能粘图、打 `@` 选文件、打 `/` 选命令、
  可以排队提交、可以切换模式。回答是流式出来的，带 markdown 渲染、工具调用分组、
  思考过程、计划审批和权限弹窗。另有一整套对话框：初次设置、设置面板、恢复会话、
  回退改动、快速打开、搜历史、全局搜索、任务、后台任务、团队、agent。
- **它能调的工具**。Bash / PowerShell、Read / Write / Edit、Glob / Grep、Sleep、
  TaskCreate / TaskUpdate / TaskList / TaskGet / TaskStop、Agent、SkillTool、
  AskUserQuestion、SendMessage、EnterPlanMode / ExitPlanMode、ToolSearch、
  Team{Create,Delete,Files,Mailbox,Manager}、Worktree，再加上 MCP 工具
  （stdio、Streamable HTTP、旧的 SSE）。
- **支持的 provider**。Anthropic、OpenAI 和 OpenAI-Responses，都支持流式输出、
  上下文压缩和裁剪，会话标题也是自动起的。
- **权限、钩子、沙箱**。对执行 shell、读写文件、抓网页、调 skill 分别设允许或
  拒绝；可以自己写钩子；沙箱越界了会报给你。
- **Skill、agent、记忆**。内置的和你自己写的 skill 都能用；可以派 agent 去后台
  干活，也有一个协调者管着它们；它还会记住东西，下次用得上的时候自己捞出来。
- **在别的机器上干活**。通过 ssh 对另一台机器上的项目开会话。第一次连的时候会在
  对面装一份 server，之后 shell、读写文件、git 都在那边跑，你这台机器只管界面和
  聊天记录。先 `rebon remote add`，再 `rebon --remote <name>`。
- **ACP 服务**。让编辑器通过 stdio JSON-RPC 驱动 Rebon：`initialize`、
  `session/new`、`session/load`、`session/list`、`session/prompt`、
  `session/cancel`，权限询问走反向 RPC，工具输出是流式的。

## 常用操作

```bash
# 在当前仓库开一个新会话
rebon

# 接着上次的会话继续（在 TUI 里按 Tab 可以翻）
rebon --resume <session-id>

# 接进编辑器（stdio 由编辑器管）
rebon --acp

# 不动配置文件，临时换 provider / 模型
rebon --provider anthropic --model claude-opus-4-7
```

TUI 里：

- `?`：看快捷键
- `/`：选命令
- `@`：选文件或符号
- `Tab`：在面板和列表之间切
- `Esc`：取消当前这步，或关掉对话框

## 更新

`rebon` 自带更新器，启动时会看一眼有没有新版本。你也可以自己重装：

```bash
npm install -g @rebon/cli@latest
```

## 隐私

Rebon 不做遥测：没有 analytics，没有崩溃上报，没有使用事件，没有匿名安装 ID。
它自己主动发出的请求只有上面那个版本检查：一个纯 `GET`，读 `@rebon/cli` 的公开
npm 元数据，不带任何能认出你的信息，连你现在装的是哪个版本都不告诉对面。除此
之外所有往外发的流量，都是你自己让它发的——问一次模型、抓一个网页、调一次 MCP。

[PRIVACY.zh-CN.md](PRIVACY.zh-CN.md) 把这个二进制可能发出的请求一条条列了出来，
写明每条在哪个文件里，以及你自己动手核实的命令。

## 排查

- **"missing optional platform package"**：去掉 `--omit=optional` /
  `--no-optional` / `--ignore-optional` 重装。
- **"unsupported platform"**：你的 `process.platform` / `process.arch` 不在支持
  的那五个里。
- **TUI 显示错乱**：看看终端支不支持真彩色和 Unicode 字宽（新版 Windows
  Terminal、iTerm2、Alacritty、WezTerm、Kitty 都可以）。老的 `cmd.exe` 和
  `conhost` 不行。
- **想看日志**：`$REBON_LOG_DIR/rebon.log`（TUI）或 stderr（`--acp`）。

## Friends

[Linux.Do](https://linux.do) — A new ideal community

## 许可

采用 [Apache License 2.0](../LICENSE)。

再分发时需随附许可证和 [NOTICE](../NOTICE) 里的署名声明，改动过的文件需标注已被修改。

第三方组件及其许可证列在 [THIRD_PARTY_NOTICES.txt](../THIRD_PARTY_NOTICES.txt)。
