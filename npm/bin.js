#!/usr/bin/env node

"use strict";

const { spawn } = require("child_process");
const path = require("path");
const fs = require("fs");

const BIN_NAME = "cc-proxy";

function getBinaryPath() {
  const ext = process.platform === "win32" ? ".exe" : "";
  const localBin = path.join(__dirname, "bin", `${BIN_NAME}${ext}`);

  if (fs.existsSync(localBin)) {
    return localBin;
  }

  // Fallback: check if binary is in PATH
  try {
    const which = process.platform === "win32" ? "where" : "which";
    const { execSync } = require("child_process");
    const result = execSync(`${which} ${BIN_NAME}`, { encoding: "utf-8" }).trim();
    if (result) return result.split("\n")[0];
  } catch (_) {
    // not in PATH
  }

  console.error(
    `[cc-proxy] ERROR: Binary not found.\n` +
      `[cc-proxy] 错误：未找到二进制文件。\n\n` +
      `  Expected location / 预期路径: ${localBin}\n\n` +
      `  Try reinstalling / 尝试重新安装:\n` +
      `    npm install cc-proxy\n\n` +
      `  Or build from source / 或从源码构建:\n` +
      `    git clone https://github.com/fengshao1227/cc-proxy.git\n` +
      `    cd cc-proxy && cargo build --release\n`
  );
  process.exit(1);
}

const binaryPath = getBinaryPath();

// Pass all arguments except node and this script
const args = process.argv.slice(2);

const child = spawn(binaryPath, args, {
  stdio: "inherit",
  env: process.env,
});

child.on("error", (err) => {
  if (err.code === "EACCES") {
    console.error(
      `[cc-proxy] Permission denied. Try: chmod +x "${binaryPath}"\n` +
        `[cc-proxy] 权限不足。请尝试: chmod +x "${binaryPath}"`
    );
  } else {
    console.error(`[cc-proxy] Failed to start: ${err.message}`);
    console.error(`[cc-proxy] 启动失败: ${err.message}`);
  }
  process.exit(1);
});

child.on("close", (code) => {
  process.exit(code !== null ? code : 1);
});
