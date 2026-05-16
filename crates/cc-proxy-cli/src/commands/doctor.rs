//! cc-proxy doctor — 体检 + 一键修复
//!
//! 默认模式：
//!   1. 显示 cc-proxy 配置摘要（端口/上游/认证）
//!   2. 检测端口当前状态（保留 / 占用 / 可用）
//!   3. 检测代理是否在跑
//!   4. 输出 actionable 建议
//!
//! `--fix` 模式（仅 Windows）：
//!   1. 检测目标端口是否被 Windows 系统保留
//!   2. 如果是 → 自动提权（UAC）→ 跑修复三步骤 → 永久排除
//!   3. 如果不是 → 输出实际占用源 + 建议

use anyhow::Result;
use console::style;

use cc_proxy_core::config::ProxyConfig;

use crate::daemon::{is_process_alive, read_pid};
use crate::diagnostics::{diagnose_port, print_diagnosis, PortDiagnosis};
#[cfg(target_os = "windows")]
use crate::diagnostics::{
    fix_windows_port_reservation, is_elevated, parse_windows_excluded_ports, relaunch_as_admin,
};

/// 加载配置：优先 config.json，其次 env/.env
fn load_config() -> Option<ProxyConfig> {
    let config_path = ProxyConfig::default_config_path();
    if config_path.exists() {
        if let Ok(c) = ProxyConfig::load_from_file(&config_path) {
            return Some(c);
        }
    }
    ProxyConfig::load().ok()
}

pub async fn run(fix: bool) -> Result<()> {
    if fix {
        return run_fix();
    }
    run_check().await
}

// ═══════════════════════════════════════════════════════════════════
//  默认体检模式
// ═══════════════════════════════════════════════════════════════════

async fn run_check() -> Result<()> {
    print_header("cc-proxy 体检");

    // ── Section 1: 配置 ──
    let config = load_config();
    let port = config.as_ref().map(|c| c.port).unwrap_or(8082);

    print_section("配置");
    match &config {
        Some(c) => {
            println!("  {} {}", style("✔").green(), style("配置文件正常").white());
            println!(
                "    {}  {}:{}",
                style("监听:").dim(),
                c.host,
                style(c.port).cyan()
            );
            println!(
                "    {}  {}",
                style("上游:").dim(),
                style(&c.openai_base_url).white()
            );
            println!(
                "    {}  {}",
                style("认证:").dim(),
                if c.anthropic_api_key.is_some() {
                    style("已启用").green().to_string()
                } else {
                    style("未启用").yellow().to_string()
                }
            );
        }
        None => {
            println!("  {} 未找到配置文件", style("✗").red().bold());
            println!(
                "    {} 运行 {} 完成首次配置",
                style("→").dim(),
                style("cc-proxy setup").cyan()
            );
            println!(
                "    {} 默认端口将使用 {}",
                style("→").dim(),
                style(port).cyan()
            );
        }
    }
    println!();

    // ── Section 2: 进程状态 ──
    print_section("代理进程");
    let proxy_running = check_proxy_running(port).await;
    if proxy_running {
        println!("  {} {}", style("✔").green(), style("代理正在运行").white());
        if let Some(pid) = read_pid() {
            println!("    {}  {}", style("PID:").dim(), style(pid).cyan());
        }
    } else {
        println!("  {} {}", style("○").dim(), style("代理未运行").dim());
        if let Some(pid) = read_pid() {
            if is_process_alive(pid) {
                println!(
                    "    {} PID 文件指向进程 {} 但 /health 无响应",
                    style("⚠").yellow(),
                    style(pid).cyan()
                );
            } else {
                println!(
                    "    {} 发现残留 PID 文件 (PID {} 已退出)",
                    style("⚠").yellow(),
                    style(pid).cyan()
                );
            }
        }
    }
    println!();

    // ── Section 3: 端口诊断 ──
    print_section(&format!("端口 {port}"));
    let diagnosis = diagnose_port(port);
    match &diagnosis {
        PortDiagnosis::WindowsReserved { .. } => {
            print_diagnosis(&diagnosis);
        }
        PortDiagnosis::HeldByProcess { pid, name, .. } => {
            // 如果占用者就是 cc-proxy 自己 → 正常
            let is_self = read_pid().map(|p| p == *pid).unwrap_or(false);
            if is_self {
                println!(
                    "  {} 端口被 cc-proxy 自身持有 (PID {} - {})",
                    style("✔").green(),
                    style(pid).cyan(),
                    name
                );
                println!();
            } else {
                print_diagnosis(&diagnosis);
            }
        }
        PortDiagnosis::Unknown { .. } => {
            if proxy_running {
                println!("  {} 端口可用，代理正常监听", style("✔").green());
                println!();
            } else {
                println!("  {} 端口可用，但代理未在运行", style("✔").green());
                println!(
                    "    {} 运行 {} 启动代理",
                    style("→").dim(),
                    style("cc-proxy start").cyan()
                );
                println!();
            }
        }
    }

    // ── Section 4: Windows 端口保留全景 ──
    #[cfg(target_os = "windows")]
    {
        let ranges = parse_windows_excluded_ports();
        if !ranges.is_empty() {
            print_section("Windows 端口保留范围（前 10 条）");
            for (i, (s, e)) in ranges.iter().take(10).enumerate() {
                let count = e - s + 1;
                let marker = if port >= *s && port <= *e {
                    style("◀ 命中").red().bold().to_string()
                } else {
                    String::new()
                };
                println!(
                    "  {:>2}. {} - {}  ({} 个)  {}",
                    i + 1,
                    style(s).cyan(),
                    style(e).cyan(),
                    style(count).dim(),
                    marker
                );
            }
            if ranges.len() > 10 {
                println!("  {} 共 {} 条范围", style("...").dim(), ranges.len());
            }
            println!();
        }
    }

    // ── 可达性提示 ──
    if matches!(diagnosis, PortDiagnosis::WindowsReserved { .. }) {
        println!(
            "  {} 运行 {} 一键永久修复",
            style("⚡").yellow().bold(),
            style("cc-proxy doctor --fix").cyan().bold()
        );
        println!();
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  --fix 模式
// ═══════════════════════════════════════════════════════════════════

#[cfg(target_os = "windows")]
fn run_fix() -> Result<()> {
    print_header("cc-proxy doctor --fix");

    let config = load_config();
    let port = config.as_ref().map(|c| c.port).unwrap_or(8082);

    println!("  目标端口: {}", style(port).cyan().bold());
    println!();

    // 1. 先确认端口确实落入保留区
    let diagnosis = diagnose_port(port);
    match &diagnosis {
        PortDiagnosis::WindowsReserved { reserved_range, .. } => {
            println!(
                "  {} 确认端口 {} 落入保留范围 [{} - {}]",
                style("✔").green(),
                style(port).cyan(),
                reserved_range.0,
                reserved_range.1
            );
            println!();
        }
        PortDiagnosis::HeldByProcess { pid, name, .. } => {
            println!(
                "  {} 端口 {} 不在 Windows 保留区，而是被进程占用:",
                style("ℹ").blue().bold(),
                style(port).cyan()
            );
            println!("    PID {}  ({})", style(pid).cyan(), style(name).white());
            println!();
            println!(
                "  {} doctor --fix 仅修复系统端口保留，不会终止其他进程",
                style("提示:").dim()
            );
            println!("    {} 请先手动终止该进程，或更换端口", style("→").dim());
            println!();
            return Ok(());
        }
        PortDiagnosis::Unknown { .. } => {
            println!(
                "  {} 端口 {} 当前可用，无需修复",
                style("✔").green(),
                style(port).cyan()
            );
            println!();
            return Ok(());
        }
    }

    // 2. 提权检查
    if !is_elevated() {
        println!(
            "  {} 此操作需要管理员权限，正在请求 UAC 授权...",
            style("⚡").yellow()
        );
        println!();
        match relaunch_as_admin(&["doctor", "--fix"]) {
            Ok(()) => {
                println!();
                println!(
                    "  {} 修复进程已在管理员窗口中执行完毕",
                    style("✔").green().bold()
                );
                return Ok(());
            }
            Err(e) => {
                println!("  {} 提权失败: {e}", style("✗").red().bold());
                println!();
                println!(
                    "  {} 请手动以管理员身份打开 cmd，执行:",
                    style("回退方案:").dim()
                );
                println!("    {}", style("net stop winnat").dim());
                println!(
                    "    {}",
                    style(format!(
                        "netsh int ipv4 add excludedportrange protocol=tcp startport={port} numberofports=1"
                    ))
                    .dim()
                );
                println!("    {}", style("net start winnat").dim());
                return Ok(());
            }
        }
    }

    // 3. 已经是管理员，直接执行
    println!(
        "  {} 已具备管理员权限，开始修复...",
        style("⚡").yellow().bold()
    );
    println!();

    match fix_windows_port_reservation(port) {
        Ok(()) => {
            println!();
            println!(
                "  {} {}",
                style("✔").green().bold(),
                style("修复成功").green().bold()
            );
            println!(
                "    端口 {} 已永久加入排除列表，重启电脑也不会再被保留",
                style(port).cyan()
            );
            println!();

            // 4. 验证
            let after = diagnose_port(port);
            match after {
                PortDiagnosis::WindowsReserved { .. } => {
                    println!(
                        "  {} 验证失败：端口仍在保留区，可能需要手动重启 winnat",
                        style("⚠").yellow()
                    );
                }
                _ => {
                    println!(
                        "  {} 验证通过：端口 {} 已可用",
                        style("✔").green(),
                        style(port).cyan()
                    );
                    println!();
                    println!(
                        "  {} 现在可以运行 {} 启动代理",
                        style("→").dim(),
                        style("cc-proxy start").cyan().bold()
                    );
                }
            }
            println!();
        }
        Err(e) => {
            println!();
            println!("  {} 修复失败: {e}", style("✗").red().bold());
            println!();
        }
    }

    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn run_fix() -> Result<()> {
    print_header("cc-proxy doctor --fix");
    println!(
        "  {} --fix 模式仅在 Windows 上可用",
        style("ℹ").blue().bold()
    );
    println!();
    println!(
        "  {} 因为只有 Windows 才有 Hyper-V/WSL2 动态端口保留问题。",
        style("说明:").dim()
    );
    println!(
        "  {} 在 macOS / Linux 上端口被占用通常是有进程在用，",
        style("    ").dim()
    );
    println!(
        "  {}  运行 {} 查看占用源。",
        style("    ").dim(),
        style("cc-proxy doctor").cyan()
    );
    println!();
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════

async fn check_proxy_running(port: u16) -> bool {
    let url = format!("http://localhost:{port}/health");
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn print_header(title: &str) {
    println!();
    println!(
        "  {}── {} ──{}",
        style("─────").dim(),
        style(title).bold().cyan(),
        style("─────").dim()
    );
    println!();
}

fn print_section(title: &str) {
    println!(
        "  {} {}",
        style("▸").cyan().bold(),
        style(title).bold().white()
    );
}
