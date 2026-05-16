#!/usr/bin/env node

"use strict";

const https = require("https");
const http = require("http");
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");

const REPO = "fengshao1227/cc-proxy";
const BIN_NAME = "cc-proxy";

// Platform mapping: Node.js -> binary suffix
const PLATFORM_MAP = {
  darwin: "darwin",
  linux: "linux",
  win32: "windows",
};

// Arch mapping: Node.js -> binary suffix
const ARCH_MAP = {
  x64: "x86_64",
  arm64: "arm64",
};

function getPlatformBinaryName() {
  const platform = PLATFORM_MAP[process.platform];
  const arch = ARCH_MAP[process.arch];

  if (!platform) {
    throw new Error(
      `Unsupported platform: ${process.platform}\n` +
        `不支持的操作系统: ${process.platform}\n` +
        `Supported: darwin (macOS), linux, win32 (Windows)`
    );
  }

  if (!arch) {
    throw new Error(
      `Unsupported architecture: ${process.arch}\n` +
        `不支持的架构: ${process.arch}\n` +
        `Supported: x64, arm64`
    );
  }

  // Windows uses x86_64 only for now
  if (process.platform === "win32" && process.arch !== "x64") {
    throw new Error(
      `Windows only supports x64 architecture.\n` +
        `Windows 目前仅支持 x64 架构。`
    );
  }

  // Linux arm64 not built yet in CI
  if (process.platform === "linux" && process.arch === "arm64") {
    throw new Error(
      `Linux arm64 binaries are not yet available. Please build from source.\n` +
        `Linux arm64 二进制文件暂未提供，请从源码构建。`
    );
  }

  const ext = process.platform === "win32" ? ".exe" : "";
  return `${BIN_NAME}-${platform}-${arch}${ext}`;
}

function getInstallDir() {
  return path.join(__dirname, "bin");
}

function getLocalBinaryPath() {
  const ext = process.platform === "win32" ? ".exe" : "";
  return path.join(getInstallDir(), `${BIN_NAME}${ext}`);
}

/**
 * Follow redirects and download a URL to a file.
 */
function download(url, destPath, redirectCount) {
  if (redirectCount === undefined) redirectCount = 0;
  if (redirectCount > 10) {
    throw new Error("Too many redirects / 重定向次数过多");
  }

  return new Promise((resolve, reject) => {
    const client = url.startsWith("https") ? https : http;
    const req = client.get(url, { headers: { "User-Agent": "cc-proxy-npm-install" } }, (res) => {
      // Handle redirects (301, 302, 307, 308)
      if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
        let redirectUrl = res.headers.location;
        if (redirectUrl.startsWith("/")) {
          const parsed = new URL(url);
          redirectUrl = `${parsed.protocol}//${parsed.host}${redirectUrl}`;
        }
        res.resume();
        return resolve(download(redirectUrl, destPath, redirectCount + 1));
      }

      if (res.statusCode !== 200) {
        res.resume();
        return reject(
          new Error(
            `Failed to download binary (HTTP ${res.statusCode}).\n` +
              `下载二进制文件失败 (HTTP ${res.statusCode}).\n` +
              `URL: ${url}`
          )
        );
      }

      const fileStream = fs.createWriteStream(destPath);
      res.pipe(fileStream);
      fileStream.on("finish", () => {
        fileStream.close(resolve);
      });
      fileStream.on("error", (err) => {
        fs.unlink(destPath, () => {});
        reject(err);
      });
    });

    req.on("error", (err) => {
      reject(
        new Error(
          `Network error downloading binary: ${err.message}\n` +
            `下载二进制文件时网络错误: ${err.message}\n` +
            `Please check your internet connection.`
        )
      );
    });

    req.setTimeout(120000, () => {
      req.destroy();
      reject(
        new Error(
          `Download timed out (120s).\n` + `下载超时 (120秒)。`
        )
      );
    });
  });
}

async function main() {
  const binaryName = getPlatformBinaryName();
  const version = require("./package.json").version;
  const downloadUrl = `https://github.com/${REPO}/releases/download/v${version}/${binaryName}`;
  const latestUrl = `https://github.com/${REPO}/releases/latest/download/${binaryName}`;

  const installDir = getInstallDir();
  const binaryPath = getLocalBinaryPath();

  // Ensure install directory exists
  fs.mkdirSync(installDir, { recursive: true });

  console.log(`[cc-proxy] Downloading binary for ${process.platform}-${process.arch}...`);
  console.log(`[cc-proxy] 正在下载 ${process.platform}-${process.arch} 平台的二进制文件...`);

  // Try version-specific URL first, then fall back to latest
  try {
    await download(downloadUrl, binaryPath);
  } catch (versionErr) {
    console.log(`[cc-proxy] Version v${version} not found, trying latest release...`);
    console.log(`[cc-proxy] 未找到 v${version} 版本，尝试最新版本...`);
    try {
      await download(latestUrl, binaryPath);
    } catch (latestErr) {
      console.error(
        `\n[cc-proxy] ERROR: Failed to download binary.\n` +
          `[cc-proxy] 错误：下载二进制文件失败。\n\n` +
          `  Attempted URLs:\n` +
          `    1. ${downloadUrl}\n` +
          `    2. ${latestUrl}\n\n` +
          `  Possible causes / 可能的原因:\n` +
          `    - No release found for your platform / 没有适配您平台的发布版本\n` +
          `    - Network issues / 网络问题\n` +
          `    - GitHub API rate limit / GitHub API 限流\n\n` +
          `  You can build from source / 您可以从源码构建:\n` +
          `    git clone https://github.com/${REPO}.git\n` +
          `    cd cc-proxy && cargo build --release\n`
      );
      process.exit(1);
    }
  }

  // Make binary executable on Unix
  if (process.platform !== "win32") {
    fs.chmodSync(binaryPath, 0o755);
  }

  console.log(`[cc-proxy] Binary installed successfully!`);
  console.log(`[cc-proxy] 二进制文件安装成功！`);
  console.log(`[cc-proxy] Location / 路径: ${binaryPath}`);
}

main().catch((err) => {
  console.error(`[cc-proxy] Installation failed: ${err.message}`);
  console.error(`[cc-proxy] 安装失败: ${err.message}`);
  process.exit(1);
});
