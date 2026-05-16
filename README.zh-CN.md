[English](README.md) | 简体中文

# cc-proxy

**让 Claude Code 接入任意 OpenAI 兼容 API。** Rust 编写的单文件代理，6.4MB，实时转换 Claude API 请求为 OpenAI 格式。

```
Claude Code ──► cc-proxy (localhost:8082) ──► 你的 API (OpenAI / 第三方中转)
```

## 快速开始

```bash
# 安装
npm i -g ccproxy-cli

# 运行（交互式菜单）
cc-proxy
```

菜单会引导你完成配置和启动，无需手动编辑任何文件。

### 或者：下载二进制

从 [GitHub Releases](https://github.com/fengshao1227/cc-proxy/releases) 下载对应平台的文件，然后：

```bash
chmod +x cc-proxy
./cc-proxy
```

### 连接 Claude Code

代理启动后，在另一个终端：

```bash
ANTHROPIC_BASE_URL=http://localhost:8082 \
ANTHROPIC_API_KEY="你的鉴权密钥" \
ANTHROPIC_AUTH_TOKEN="" \
claude
```

> 鉴权密钥在配置完成后会显示，也可以在菜单的「连接信息」中查看。

> 如果你登录了 claude.ai，必须加 `ANTHROPIC_AUTH_TOKEN=""` 才能让 Claude Code 走代理。

## 功能特性

- **6.4MB 单文件** — 不需要 Python、Docker，下载即用
- **交互式菜单** — 配置、启动、停止、状态、连接信息，全部菜单操作
- **Per-tier 模型映射** — opus / sonnet / haiku 分别配置不同模型
- **Per-tier 思考模式** — 每个级别独立设置思考强度 (none/low/medium/high/xhigh)
- **完整 Tool Use** — Read、Write、Bash、Grep 等工具调用全部正常 (GPT-5.4 实测通过)
- **流式输出** — 实时逐 token SSE 转换
- **自动生成鉴权密钥** — setup 时自动生成，无需手动设置
- **后台运行** — daemon 模式 + PID 管理

## 交互式菜单

直接运行 `cc-proxy`（无参数）进入菜单：

```
  ┌─────────────────────────────────────────────────────┐
  │              cc-proxy                               │
  │        Claude Code ↔ Any LLM Provider               │
  │        v0.1.6   |   Rust   |   6.4MB                │
  └─────────────────────────────────────────────────────┘

  ● 代理运行中

  选择操作:
  🔄  重启代理     — 停止后重新启动
  🔑  连接信息     — 查看地址和密钥
  📊  查看状态     — 运行中
  🔗  测试连接     — 测试上游 API
  ⏹   停止代理
  ⚙   配置向导     — 修改配置
  Q   退出
```

## CLI 命令（脚本 / Linux 服务器用）

| 命令 | 说明 |
|------|------|
| `cc-proxy` | 交互式菜单（默认） |
| `cc-proxy setup` | 配置向导 |
| `cc-proxy start` | 前台启动 |
| `cc-proxy start -d` | 后台启动 |
| `cc-proxy stop` | 停止后台代理 |
| `cc-proxy status` | 查看配置和状态 |
| `cc-proxy test` | 测试上游连通性 |
| `cc-proxy doctor` | 体检：配置 / 进程 / 端口诊断 |
| `cc-proxy doctor --fix` | **Windows 一键修复端口保留**（需 UAC 提权） |

## 配置说明

配置向导只问三件事：

1. **API 地址 + Key** — 你的 OpenAI 兼容 API 端点
2. **模型选择** — 每个级别从预设列表选（GPT-5.4、5.1 等）或手动输入
3. **思考强度** — 每个级别独立设置 (none/low/medium/high/xhigh)

配置保存在 `~/.cc-proxy/config.json`（0600 权限）。

### 模型映射

| Claude Code 请求 | cc-proxy 转发到 |
|------------------|----------------|
| `*opus*` | BIG_MODEL |
| `*sonnet*` | MIDDLE_MODEL |
| `*haiku*` | SMALL_MODEL |
| 非 Claude 模型 | 原样透传 |

### Per-tier 配置示例

```
BIG   (opus)   → gpt-5.4      思考强度: xhigh
MIDDLE(sonnet) → gpt-5.4      思考强度: medium
SMALL (haiku)  → gpt-5.4-mini 思考强度: none
```

### 环境变量（替代配置向导）

| 变量 | 默认值 | 说明 |
|------|--------|------|
| `OPENAI_API_KEY` | *必填* | API Key |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | API 地址 |
| `BIG_MODEL` | `gpt-4o` | opus 映射模型 |
| `MIDDLE_MODEL` | *(同 BIG)* | sonnet 映射模型 |
| `SMALL_MODEL` | `gpt-4o-mini` | haiku 映射模型 |
| `BIG_REASONING` | `none` | opus 思考强度 |
| `MIDDLE_REASONING` | `none` | sonnet 思考强度 |
| `SMALL_REASONING` | `none` | haiku 思考强度 |
| `PORT` | `8082` | 代理端口 |
| `ANTHROPIC_API_KEY` | *无* | 客户端鉴权密钥 |

## 常见问题

**Claude Code 报 "model not exist"**
→ 检查代理是否在运行 (`cc-proxy` → 查看状态)，以及 `ANTHROPIC_API_KEY` 是否和代理配置一致。

**Auth conflict 错误**
→ 加 `ANTHROPIC_AUTH_TOKEN=""` 环境变量，强制走 API Key 而不是 claude.ai 登录。

**如何每次自动走代理？**
→ 把环境变量写入 `~/.zshrc`（菜单「连接信息」会显示具体命令）。

**Windows: 重启电脑后 `cc-proxy start` 报「通常每个套接字地址(协议/网络地址/端口)只允许使用一次」？**

这不是 cc-proxy 的问题，配置文件也不会丢，**端口不需要换**。

真正的原因：Windows 上 Hyper-V / WSL2 / Docker Desktop 通过 winnat 服务从 TCP 动态端口范围里"保留"了一批端口。**重启电脑后保留范围会变化**，这次正好把 8082 圈进去了。一旦命中：
- 任何用户态程序都 bind 失败（错误码 10048）
- `netstat` 看不到任何进程在监听
- cc-proxy `/health` 探测自然也连不上 → 看起来"未运行"

**一键永久修复**（v0.2.3+）：

```bash
cc-proxy doctor --fix
```

会自动弹 UAC 授权，然后把 8082 永久加入 Windows 端口排除列表（`net stop winnat` → `netsh add excludedportrange` → `net start winnat`）。修复后**重启电脑也不会再复发**。

注意：`net stop winnat` 会短暂中断 Docker/WSL2 网络（~3秒）。

**先看看到底什么情况？**

```bash
cc-proxy doctor
```

体检模式会输出当前配置 / 进程 / 端口三方诊断，列出 Windows 端口保留范围的前 10 条，命中目标端口的会用 `◀ 命中` 高亮。

## 社区

本项目在 [LINUX DO](https://linux.do/) 社区分享。

## License

[MIT](LICENSE)
