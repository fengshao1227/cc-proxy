# cc-proxy

A high-performance proxy for Claude API, built with Rust.

## Install

```bash
npm install -g cc-proxy
```

## Usage

```bash
# Start the proxy server
cc-proxy start

# Show help
cc-proxy --help
```

## What is this?

This npm package is a thin wrapper around the native `cc-proxy` binary built in Rust. On `npm install`, it automatically downloads the correct pre-built binary for your platform (macOS, Linux, Windows) and architecture (x64, arm64).

## Supported Platforms

| Platform | Architecture | Status |
|----------|-------------|--------|
| macOS    | x64 (Intel) | Supported |
| macOS    | arm64 (Apple Silicon) | Supported |
| Linux    | x64         | Supported |
| Windows  | x64         | Supported |

## Building from Source

If no pre-built binary is available for your platform:

```bash
git clone https://github.com/fengshao1227/cc-proxy.git
cd cc-proxy
cargo build --release
```

The binary will be at `target/release/cc-proxy`.

## Links

- [GitHub Repository](https://github.com/fengshao1227/cc-proxy)
- [Report Issues](https://github.com/fengshao1227/cc-proxy/issues)

## License

MIT
