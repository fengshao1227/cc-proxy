[English](README.md) | [简体中文](README.zh-CN.md)

# cc-proxy

**Use any OpenAI-compatible API with Claude Code.** A single 6.4MB Rust binary that translates Claude API requests into OpenAI format in real-time.

```
Claude Code ──► cc-proxy (localhost:8082) ──► Your API (OpenAI / third-party)
```

## Quick Start

```bash
# Install
npm i -g ccproxy-cli

# Run (interactive menu)
cc-proxy
```

That's it. The interactive menu guides you through configuration and startup.

### Alternative: Download Binary

Grab the latest binary from [GitHub Releases](https://github.com/fengshao1227/cc-proxy/releases), then:

```bash
chmod +x cc-proxy
./cc-proxy
```

### Connect Claude Code

Once cc-proxy is running, open another terminal:

```bash
ANTHROPIC_BASE_URL=http://localhost:8082 \
ANTHROPIC_API_KEY="your-auth-key" \
ANTHROPIC_AUTH_TOKEN="" \
claude
```

> The auth key is shown after setup and available via the "Connection Info" menu option.

## Features

- **6.4MB single binary** — no Python, no Docker, no runtime dependencies
- **Interactive TUI** — menu-driven setup, start, stop, status, connection info
- **Per-tier model mapping** — configure different models for opus / sonnet / haiku
- **Per-tier reasoning** — independent thinking effort (none/low/medium/high/xhigh) per model level
- **Full tool use** — Read, Write, Bash, Grep etc. all work correctly (GPT-5.4 verified)
- **Streaming SSE** — real-time token-level streaming conversion
- **Auto auth key** — random authentication key generated on setup
- **Daemon mode** — background process with PID management
- **Graceful shutdown** — SIGTERM/SIGINT with connection draining

## Interactive Menu

Running `cc-proxy` without arguments enters the interactive menu:

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

## CLI Commands (for scripts / Linux)

| Command | Description |
|---------|-------------|
| `cc-proxy` | Interactive menu (default) |
| `cc-proxy setup` | Configuration wizard |
| `cc-proxy start` | Start proxy (foreground) |
| `cc-proxy start -d` | Start as daemon |
| `cc-proxy stop` | Stop daemon |
| `cc-proxy status` | Show config and status |
| `cc-proxy test` | Test upstream API |
| `cc-proxy doctor` | Diagnose port / config / process |
| `cc-proxy doctor --fix` | **Windows: one-click fix port reservation** (UAC) |

## Configuration

Setup wizard asks three things:

1. **API URL + Key** — your OpenAI-compatible endpoint
2. **Models** — pick from presets (GPT-5.4, GPT-5.1, etc.) or type custom, per tier
3. **Reasoning** — thinking effort per tier (none/low/medium/high/xhigh)

Config saved to `~/.cc-proxy/config.json` (0600 permissions).

### Model Mapping

| Claude Code requests... | cc-proxy maps to... |
|-------------------------|---------------------|
| `*opus*` | BIG_MODEL |
| `*sonnet*` | MIDDLE_MODEL |
| `*haiku*` | SMALL_MODEL |
| Non-Claude models | Pass-through |

### Per-tier Reasoning Example

```
BIG   (opus)   → gpt-5.4      reasoning: xhigh
MIDDLE(sonnet) → gpt-5.4      reasoning: medium
SMALL (haiku)  → gpt-5.4-mini reasoning: none
```

### Environment Variables (alternative to setup wizard)

| Variable | Default | Description |
|----------|---------|-------------|
| `OPENAI_API_KEY` | *(required)* | API key for upstream |
| `OPENAI_BASE_URL` | `https://api.openai.com/v1` | API endpoint |
| `BIG_MODEL` | `gpt-4o` | Opus model |
| `MIDDLE_MODEL` | *(BIG_MODEL)* | Sonnet model |
| `SMALL_MODEL` | `gpt-4o-mini` | Haiku model |
| `BIG_REASONING` | `none` | Reasoning for opus tier |
| `MIDDLE_REASONING` | `none` | Reasoning for sonnet tier |
| `SMALL_REASONING` | `none` | Reasoning for haiku tier |
| `PORT` | `8082` | Server port |
| `ANTHROPIC_API_KEY` | *(none)* | Client auth key |

## FAQ

**Windows: after rebooting, `cc-proxy start` fails with `Only one usage of each socket address (protocol/network address/port) is normally permitted` (error 10048)?**

Not a cc-proxy bug, your config is intact, **you don't need to change the port**.

Real cause: on Windows, Hyper-V / WSL2 / Docker Desktop reserve a slice of TCP ports through the `winnat` service. **The reservation range shifts every reboot** — this time it happened to grab 8082. Once it does:
- Any user-mode program fails to bind (WSAEADDRINUSE 10048)
- `netstat` shows nothing listening on the port
- `cc-proxy /health` probe also fails → looks like "not running"

**One-click permanent fix** (v0.2.3+):

```bash
cc-proxy doctor --fix
```

Triggers UAC elevation, then permanently adds 8082 to Windows' excluded port range (`net stop winnat` → `netsh add excludedportrange` → `net start winnat`). After this, **rebooting will not bring the issue back**.

Note: `net stop winnat` briefly interrupts Docker/WSL2 networking (~3 seconds).

**Want to inspect first?**

```bash
cc-proxy doctor
```

Health-check mode prints config / process / port diagnosis, lists the top 10 Windows reserved port ranges, and highlights the one containing your port with `◀ HIT`.

**`model not exist` error from Claude Code**
→ Check that the proxy is running (`cc-proxy` → status) and `ANTHROPIC_API_KEY` matches the proxy's auth key.

**Auth conflict error**
→ Add `ANTHROPIC_AUTH_TOKEN=""` to force the API-key path instead of the claude.ai login.

## Community

This project is shared with the [LINUX DO](https://linux.do/) community.

## License

[MIT](LICENSE)
