# ZCode 账户管家 (`zcode-account-manager`)

<p align="center">
  <b>一个界面管理多个 ZCode 账户：一键备份、快速切换、安全清理，本地状态始终可控。</b>
</p>

<p align="center">
  <a href="https://github.com/beyondcy1013/zcode-account-manager/stargazers"><img src="https://img.shields.io/github/stars/beyondcy1013/zcode-account-manager" alt="Stars Badge"/></a>
  <a href="https://github.com/beyondcy1013/zcode-account-manager/issues"><img src="https://img.shields.io/github/issues/beyondcy1013/zcode-account-manager" alt="Issues Badge"/></a>
  <a href="https://github.com/beyondcy1013/zcode-account-manager/blob/main/LICENSE"><img src="https://img.shields.io/github/license/beyondcy1013/zcode-account-manager" alt="License Badge"/></a>
</p>

---

## 📌 项目简介 (About)

在使用 **ZCode (智谱 BigModel 编程客户端 / Z.ai)** 时，很多开发者遇到以下痛点：
- **换号/切换多账号困难**：客户端经常记住旧账号的 Cookie 和 OAuth Token，无法干净退出或切换。
- **无法弹出新用户免费 Flash 套餐引导**：老账号登录过的机器残留了本地权益缓存（`coding-plan-cache.json`），即使换了新账号也不会弹出新版免费套餐领取界面。
- **环境残留与排错困难**：本地缓存损坏导致客户端无法拉取最新 Plan 或报错。

**`zcode-account-manager`**（原 `zcode-fresh-reset`）是一个原生 Windows 图形化账户管理工具。它可以为每个已登录账户保存独立的本地状态快照，在多个账户之间快速切换，同时保留原有的一键重置与安全清理能力。

## 账户备份与切换

双击 `zcode-account-manager.exe` 即进入 GUI，无需命令行：

1. 启动工具后自动识别**当前登录账号**：从本地配置提取用户名 / 用户 ID / 套餐来源，并生成凭据指纹，显示在“当前账号”栏。
2. 点击 **保存当前账户** 即可，名称留空时自动使用账号标识命名（如 `bigmodel-user`）；想用更好记的名字就填一个**别名**。
3. 登录并保存其他账户，列表会集中展示所有账户快照（名称、账号标识、保存时间）。
4. 之后随时在列表中点击 **恢复**：如 ZCode 正在运行会先自动关闭，恢复完成后可勾选**自动启动 ZCode**，无需手动重启客户端。

恢复前，工具会自动更新当前账户快照并创建安全备份；目标账户恢复失败时会自动回滚。账户凭据不会显示在界面中，快照保存在 `%USERPROFILE%\.zcode\account_backups\`。每个账户可随时通过“别名”按钮重命名显示名称，别名只影响展示，不影响自动命名。

> 账户快照包含登录凭据和 Cookie，请像保护密码一样保护备份目录，不要上传或分享。

> 提示：恢复后自动启动 ZCode 依赖找到客户端程序（默认探测 `%LOCALAPPDATA%\Programs\ZCode\ZCode.exe` 等常见位置）。若安装位置特殊，可设置环境变量 `ZCODE_APP_PATH` 指向 ZCode 主程序。命令行 `zcode-account-manager.exe --whoami` 可快速查看当前识别到的账号标识。

![ZCode 账户管家 GUI](docs/zcode-account-manager.png)

---

## CLI 账号管理（Gemini / Codex / Claude Code）

「CLI 账号」页为三个主流 AI 编程 CLI 提供统一的多账号体验：**Google Gemini CLI / Antigravity CLI（`agy`）**、**OpenAI Codex CLI** 和 **Anthropic Claude Code**。每个工具一个标签页，操作方式完全一致：

1. 用 CLI 登录一个账号后，在对应标签页工具会自动识别当前账号（邮箱 / 认证方式 / 凭据指纹）并显示在「当前账号」栏。
2. 点击 **保存当前账号** 创建备份；名称留空时自动用邮箱前缀或指纹前缀命名，也可填一个好记的**别名**。
3. 登录其他账号并分别保存，之后随时在列表中点击 **切换**：切换前当前状态会自动备份到原账号，目标恢复失败时自动回滚。
4. 点击 **清空账号** 可一键退出登录：先自动备份当前登录状态，再清除凭据文件，恢复到未登录的原始状态，方便换一个账号重新登录。清空产生的备份保留在列表里，随时可以切换回来。

各工具的快照内容与备份位置（备份统一放在各自目录下的 `account_backups/`）：

**Gemini / Antigravity CLI**（`~/.gemini/account_backups/`；Gemini CLI 已并入 Antigravity CLI，两者共用 `~/.gemini`，同时支持）：

| 快照项 | 路径（`~/.gemini/` 下） | 说明 |
| :--- | :--- | :--- |
| **agy OAuth 凭据** | `antigravity-cli/antigravity-oauth-token` | Antigravity CLI 登录 Token；指纹按内层 `refresh_token` 计算，token 刷新不影响账号匹配 |
| **agy 设置** | `antigravity-cli/settings.json` | agy 客户端设置（模型、权限等）；旧快照缺失此项时保留本机现状 |
| **OAuth 凭据** | `oauth_creds.json` | 旧版 Gemini CLI 登录 Token（含 id_token，用于识别邮箱） |
| **账号缓存** | `google_accounts.json` | 旧版 Gemini CLI 缓存的账号邮箱 |
| **Web 账号缓存** | `google_web_accounts.json` | Web 登录账号缓存（存在时纳入快照） |
| **客户端设置** | `settings.json` | 旧版 Gemini CLI 设置，含认证方式选择；旧快照缺失此项时保留本机现状 |
| **ADC 凭据** | `application_default_credentials.json` | Vertex AI / 应用默认凭据（存在时纳入快照） |

> 邮箱识别：旧版 Gemini CLI 从 `google_accounts.json` / id_token 读取；`agy` 的 token 文件不含邮箱，改为从其认证日志（`antigravity-cli/log/` 中的 `applyAuthResult: email=...`）尽力提取，日志被清理时仅影响显示名称，不影响凭据指纹与账号匹配。

> **Windows 版 agy ≥ 1.2.0**：登录 Token 存放在 Windows 凭据管理器（keyring，条目 `gemini:antigravity`），本地没有 token 文件。工具会从认证日志识别账号（邮箱 + 认证方式，指纹按邮箱摘要计算）；文件级快照仍可备份 agy 设置与旧版 Gemini CLI 文件，但 OAuth 登录本体不受快照管理——在 Windows 上切换/退出 Google OAuth 账号请使用 `agy` 自身的 `login` / `logout`。

**Codex CLI**（`~/.codex/account_backups/`）：

| 快照项 | 路径（`~/.codex/` 下） | 说明 |
| :--- | :--- | :--- |
| **认证凭据** | `auth.json` | ChatGPT OAuth 登录（`tokens.*`，邮箱取自 id_token，指纹按 `refresh_token` 计算）或 API Key 模式（按 Key 摘要区分账号） |
| **用户配置** | `config.toml` | 模型、Provider 等用户配置；快照缺失时保留本机现状，不随账号切换 |

**Claude Code**（`~/.claude/account_backups/`）：

| 快照项 | 路径（`~/.claude/` 下） | 说明 |
| :--- | :--- | :--- |
| **OAuth 凭据** | `.credentials.json` | claude.ai OAuth 登录（存在时纳入快照；邮箱取自 accessToken，指纹按 `refreshToken` 计算） |
| **设置** | `settings.json` | 用户设置，含 `env` 中的 `ANTHROPIC_AUTH_TOKEN` / `ANTHROPIC_API_KEY` / `ANTHROPIC_BASE_URL` 自定义端点认证——替换该文件即可在中转 / 第三方端点账号之间切换；快照缺失时保留本机现状 |

> 端点账号识别：Claude Code 通过 settings 中的 Token + `ANTHROPIC_BASE_URL` 识别（如 `API 端点 open.bigmodel.cn`），Token 或端点不同即视为不同账号。

> ⚠️ 切换前请退出对应 CLI 正在运行的会话：运行中的会话可能在切换后把旧登录凭据回写覆盖。工具检测到相关 CLI 运行时会在界面上给出提示；切换完成后重启会话即可使用新账号。

> 账户快照包含 OAuth 登录凭据与 API Token，请像保护密码一样保护备份目录，不要上传或分享。

---

## 🔍 深度原理解析 (Why & How)

### 1. 为什么“新安装客户端”会弹出领取 Flash 免费套餐？
- **本地机制**：当客户端检测不到 `credentials.json`、`session/Cookies` 和 `coding-plan-cache.json` 时，判定当前为“首次启动未初始化状态”，会自动激活新手引导及套餐领取弹窗。
- **云端校验**：免费 Flash 套餐最终由 BigModel / ZCode 云端根据账号（UID/手机号/OAuth）和设备特征进行核验下发。

### 2. 为什么登录过的账号不会再弹出？
- **本地有缓存**：`coding-plan-cache.json` 记录了旧套餐数据，客户端启动直接读缓存，不再请求新引导。
- **云端有记录**：同一个账号在服务端已被标记为“已领过”或“老用户”。

### 3. 正确的“新用户领取”操作流程
1. 完全退出 ZCode，运行 **`zcode-account-manager.exe clean`**，彻底清理本地老账号状态。
2. 启动 ZCode 客户端，此时客户端处于纯净新机状态。
3. 登录**未领取过该福利的新账号**，客户端将正常触发新用户新手引导并成功领取免费 Flash 套餐。

---

## 📂 涉及的关键路径清单

| 类别 | Windows 路径 | 作用说明 |
| :--- | :--- | :--- |
| **登录凭据** | `%USERPROFILE%\.zcode\v2\credentials.json` | 用户登录 Token 与授权身份 |
| **套餐缓存** | `%USERPROFILE%\.zcode\v2\coding-plan-cache.json` | 缓存的 Plan 权益与领取状态 |
| **遥测埋点** | `%USERPROFILE%\.zcode\v2\telemetry-state.json` | 本地客户端状态数据 |
| **Provider 配置** | `%USERPROFILE%\.zcode\v2\config.json` | 模型 Provider 列表，内置套餐条目的 apiKey 为账号 OAuth JWT（账号身份来源之一） |
| **应用设置** | `%USERPROFILE%\.zcode\v2\setting.json` | 客户端设置，含 Provider 家族/套餐选择状态 |
| **CLI 配置** | `%USERPROFILE%\.zcode\cli\config.json` | CLI 侧 Provider 配置（含账号 apiKey） |
| **网页会话** | `%APPDATA%\ZCode\session\Cookies` | Electron 登录态 Cookie |
| **页面存储** | `%APPDATA%\ZCode\session\Local Storage\` | 前端页面持久化数据 |
| **设备特征** | `%APPDATA%\ZCode\rum-electron-store\` | 客户端设备监控与特征信息 |
| **升级标记** | `%APPDATA%\ZCode\.updaterId` | 客户端更新器标识 |

> `%APPDATA%` 在非 Windows 平台的对应目录：Linux 为 `~/.config`（ZCode 桌面端位于 `~/.config/ZCode`），macOS 为 `~/Library/Application Support`。
>
> 0.6.0 之前的账户快照不包含 Provider 配置、应用设置和 CLI 配置三项；用旧快照切换账户时会保留这三项的本机现状，不做清空。

---

## 🚀 快速上手 (Quick Start)

发布文件 `zcode-account-manager.exe` 不需要 Python 或其他运行时。

直接双击 EXE 会打开 **ZCode 账户管家**。账户页用于备份、更新、切换和删除 ZCode 账户快照；「CLI 账号」页为 Gemini / Antigravity、Codex、Claude Code 三个 CLI 提供同样的多账号备份与切换；清理页提供安全清理和完整重置；「自动发送」页可以向 ZCode 桌面端的指定会话自动发送消息。所有会改动本地状态的操作都有明确状态反馈和二次确认。

## 自动发送消息（Linux X11）

「自动发送」页可以把一条消息自动发进 ZCode 桌面端侧栏「已置顶」区的第 N 个会话，适合定时任务、批量通知等场景。执行流程（约 5 秒，期间请勿操作鼠标键盘）：

1. 按窗口类 `ZCode` 定位并激活 ZCode 主窗口；
2. 把会话列表滚动到最顶部，露出「已置顶」区块；
3. 点击「已置顶」区从上往下第 N 个会话，并点击输入框使其获得焦点；
4. 将消息写入剪贴板粘贴进输入框（绕开输入法预编辑，避免回车先确认候选词），回车发送。

**定位测试自定义**：始终包含「激活窗口 + 滚动到顶部 + 悬停目标行」，测试项目可通过多选框自由组合——「点击切换」「粘贴输入」「回车发送」，全部不勾选即为纯定位悬停，方便逐段核对与排障。

**定时发送**：勾选「定时发送」并填入 HH:MM 时间（可选「每天重复」），确认后到点自动执行完整发送；界面会显示已预约的任务，可随时「取消定时」。定时任务在程序退出后失效。命令行同样支持：

```bash
zcode-account-manager send --pinned 1 --message "日常巡检开始"
zcode-account-manager send --pinned 2 --message "..." --dry-run          # 只定位悬停，不点击不发送
zcode-account-manager send --pinned 1 --message "早报" --at 09:30        # 今天 09:30 发送
zcode-account-manager send --pinned 1 --message "早报" --at 09:30 --daily # 每天 09:30 发送
```

发送前后会自动保存并恢复剪贴板内容。点击坐标基于默认字号/缩放实测校准，若系统缩放不同导致点击偏移，需要调整 `src/auto_send.rs` 中的坐标常量。

## 多语言与自动更新

命令行兼容模式仍保留；设置环境变量 `ZCODE_LANG=en` 后显示英文交互文本。程序内置 GitHub Release 更新地址，可运行 `zcode-account-manager.exe --check-update` 手动检查；`ZCODE_UPDATE_MANIFEST_URL` 可覆盖默认地址。清单格式：

```json
{"version":"0.5.0","url":"https://github.com/beyondcy1013/zcode-account-manager/releases/latest/download/zcode-account-manager-0.5.0"}
```

## 清理效果示例

下图为清理前的 ZCode 套餐界面示例。清理本机登录态、会话和权益缓存后，重新登录符合条件的新账号时，客户端会重新执行首次启动权益检测；最终资格仍由 ZCode 服务端账号策略决定。

![清理后同一台电脑重新出现领取入口](docs/zcode-claim-after-reset.png)

### 1. 打开图形界面
```bash
zcode-account-manager.exe
```

### 2. 查看当前状态
```bash
zcode-account-manager.exe inspect
```

### 3. 一键完整重置
> ⚠️ **注意**：执行前请确保已**完全退出 ZCode 客户端**。默认会自动在 `%USERPROFILE%\.zcode\reset_backups\` 下建立完整备份。
```bash
zcode-account-manager.exe clean
```

### 4. 安全模式（仅清理套餐缓存，不影响当前登录凭据）
```bash
zcode-account-manager.exe clean --safe
```

### 5. 仅备份配置
```bash
zcode-account-manager.exe backup
```

### 6. 从源码构建
```bash
cargo test
cargo build --release
```

生成文件位于 `target/release/zcode-account-manager`。仓库中的 GitHub Actions 也会在推送 `v*` 标签时构建 Linux x86_64 可执行文件并发布 Release。

### 7. 平台支持

| 平台 | 状态 | 说明 |
| :--- | :--- | :--- |
| Windows x86_64 | ✅ 主要目标 | GUI、系统托盘（Shell_NotifyIcon）、CLI 子命令均可使用 |
| Linux X11 | ✅ 可用 | 同一 GUI；托盘走 XEmbed 协议，CLI 子命令均可使用 |

两个平台共用同一套代码：平台差异（托盘、打开目录、进程管理、字体）都通过 `#[cfg]` 分支处理。CLI 子命令在 Windows 上从终端启动时会自动附着到父控制台，输出与交互和 Linux 一致；双击打开 GUI 则不会闪现控制台窗口。

---

## 🤝 交流与联系 (Contact & Author)

如果您在使用过程中遇到任何问题，或者有功能建议，欢迎联系与交流：

- **GitHub Issues**: [提交 Issue](https://github.com/beyondcy1013/zcode-account-manager/issues)
- **GitHub 主页**: [@beyondcy1013](https://github.com/beyondcy1013)
- **项目仓库**: [https://github.com/beyondcy1013/zcode-account-manager](https://github.com/beyondcy1013/zcode-account-manager)

🌟 如果这个项目对你有帮助，欢迎在 GitHub 点个 **Star** 支持一下！

---

## 📄 开源协议 (License)

本项目采用 [MIT License](LICENSE) 协议开源。
