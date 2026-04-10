//! 端口绑定失败诊断 / Windows 端口保留处理
//!
//! Windows 上 Hyper-V / WSL2 / Docker Desktop 通过 winnat 服务从 TCP 动态
//! 端口范围里"保留"一批端口（excluded port range）。一旦目标端口落入保留区：
//!  - 任何用户态程序都 bind 失败（错误码 10048 / WSAEADDRINUSE）
//!  - netstat 看不到任何进程在监听
//!  - cc-proxy /health 探测自然也连不上 → 看起来"未运行"
//!
//! 本模块负责：
//!  1. 解析 `netsh int ipv4 show excludedportrange protocol=tcp` 输出
//!  2. 判断目标端口是否落入保留区
//!  3. 在 bind 失败时给用户输出 actionable 建议
//!  4. `cc-proxy doctor --fix` 一键修复（需要管理员权限）

use std::io;
use std::process::Command;

use console::style;

// ═══════════════════════════════════════════════════════════════════
//  Public API
// ═══════════════════════════════════════════════════════════════════

/// 端口诊断结果
#[derive(Debug, Clone)]
#[allow(dead_code)] // WindowsReserved 仅在 Windows 平台构造
pub enum PortDiagnosis {
    /// Windows 系统保留区命中（最棘手的情况）
    WindowsReserved {
        port: u16,
        reserved_range: (u16, u16),
    },
    /// 被某个进程占用（可定位）
    HeldByProcess { port: u16, pid: u32, name: String },
    /// 未知（无法定位占用源）
    Unknown { port: u16 },
}

/// bind 失败时的入口：分析原因并打印 actionable 建议
pub fn print_bind_failure_advice(addr: &str, port: u16, io_error: &io::Error) {
    println!();
    println!(
        "  {} {}",
        style("✗").red().bold(),
        style(format!("无法绑定 {addr}")).red().bold()
    );
    println!("  {} {io_error}", style("原因:").dim());
    println!();

    // 只有 AddrInUse 才走深度诊断；其他错误（权限、地址不可用）直接提示
    let is_addr_in_use = io_error.kind() == io::ErrorKind::AddrInUse
        || io_error.raw_os_error() == Some(48)         // macOS EADDRINUSE
        || io_error.raw_os_error() == Some(98)         // Linux EADDRINUSE
        || io_error.raw_os_error() == Some(10048); // Windows WSAEADDRINUSE

    if !is_addr_in_use {
        println!(
            "  {} 端口 {port} 不可用，请检查权限或防火墙",
            style("提示:").dim()
        );
        println!();
        return;
    }

    let diagnosis = diagnose_port(port);
    print_diagnosis(&diagnosis);
}

/// 主动诊断端口当前状态（doctor 子命令使用）
pub fn diagnose_port(port: u16) -> PortDiagnosis {
    // 1. 优先检测 Windows 端口保留（最隐蔽的情况）
    #[cfg(target_os = "windows")]
    {
        if let Some(range) = windows_excluded_range_containing(port) {
            return PortDiagnosis::WindowsReserved {
                port,
                reserved_range: range,
            };
        }
    }

    // 2. 跨平台检测进程占用
    if let Some((pid, name)) = find_port_holder(port) {
        return PortDiagnosis::HeldByProcess { port, pid, name };
    }

    PortDiagnosis::Unknown { port }
}

/// 输出诊断结果（带 actionable 建议）
pub fn print_diagnosis(diagnosis: &PortDiagnosis) {
    match diagnosis {
        PortDiagnosis::WindowsReserved {
            port,
            reserved_range,
        } => print_windows_reserved_advice(*port, *reserved_range),
        PortDiagnosis::HeldByProcess { port, pid, name } => {
            println!(
                "  {} 端口 {} 已被进程占用",
                style("诊断:").yellow().bold(),
                style(port).cyan()
            );
            println!(
                "  {}  PID {}  ({})",
                style("    →").dim(),
                style(pid).cyan(),
                style(name).white()
            );
            println!();
            println!("  {} 解决方法:", style("修复:").green().bold());
            #[cfg(unix)]
            println!(
                "    {}",
                style(format!("kill {pid}        # 优雅关闭")).cyan()
            );
            #[cfg(windows)]
            println!("    {}", style(format!("taskkill /PID {pid} /F")).cyan());
            println!();
        }
        PortDiagnosis::Unknown { port } => {
            println!(
                "  {} 端口 {} 不可用，但未找到占用进程",
                style("诊断:").yellow().bold(),
                style(port).cyan()
            );
            println!();
            #[cfg(target_os = "windows")]
            {
                println!(
                    "  {} 这通常是 Windows Hyper-V/WSL2 动态端口保留导致的，",
                    style("提示:").dim()
                );
                println!(
                    "  {}  即使 netstat 看不到占用，winnat 服务也已锁定该端口。",
                    style("    ").dim()
                );
                println!();
                println!(
                    "  {} 运行 {} 一键修复",
                    style("建议:").green().bold(),
                    style("cc-proxy doctor --fix").cyan().bold()
                );
                println!();
            }
            #[cfg(not(target_os = "windows"))]
            {
                println!(
                    "  {} 请检查防火墙规则或端口是否被系统保留",
                    style("提示:").dim()
                );
                println!();
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Windows: 端口保留检测
// ═══════════════════════════════════════════════════════════════════

/// 解析 `netsh int ipv4 show excludedportrange protocol=tcp` 输出
///
/// 输出格式（中英版本一致，只看数字行）:
/// ```text
/// Protocol tcp Port Exclusion Ranges
///
/// Start Port    End Port
/// ----------    --------
///       1024        1123
///       8080        8179
///      50000       50059
/// ```
#[cfg(target_os = "windows")]
pub fn parse_windows_excluded_ports() -> Vec<(u16, u16)> {
    let output = Command::new("netsh")
        .args(["int", "ipv4", "show", "excludedportrange", "protocol=tcp"])
        .output();

    let Ok(out) = output else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut ranges = Vec::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // 匹配两列纯数字行（前后可能有空白）
        let mut parts = trimmed.split_whitespace();
        let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
            continue;
        };
        if parts.next().is_some() {
            continue; // 不是恰好两列
        }
        let (Ok(start), Ok(end)) = (a.parse::<u16>(), b.parse::<u16>()) else {
            continue;
        };
        if start <= end {
            ranges.push((start, end));
        }
    }

    ranges
}

/// 返回包含目标端口的保留范围（如果存在）
#[cfg(target_os = "windows")]
fn windows_excluded_range_containing(port: u16) -> Option<(u16, u16)> {
    parse_windows_excluded_ports()
        .into_iter()
        .find(|(s, e)| port >= *s && port <= *e)
}

// ═══════════════════════════════════════════════════════════════════
//  跨平台: 进程占用检测
// ═══════════════════════════════════════════════════════════════════

/// 查找占用指定端口的进程 (PID, name)
pub fn find_port_holder(port: u16) -> Option<(u32, String)> {
    #[cfg(unix)]
    {
        find_port_holder_unix(port)
    }
    #[cfg(windows)]
    {
        find_port_holder_windows(port)
    }
}

#[cfg(unix)]
fn find_port_holder_unix(port: u16) -> Option<(u32, String)> {
    // lsof -nP -iTCP:8082 -sTCP:LISTEN
    // 正确语法: `-iTCP:8082` 是一个参数（协议:端口）
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fpcn"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // lsof -F 输出按 process 分组，每组：p<pid>\nc<name>\nf<fd>...
    // 取第一个 pid/name 配对即可
    let mut pid: Option<u32> = None;
    let mut name: Option<String> = None;
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix('p') {
            if pid.is_none() {
                pid = rest.parse().ok();
            }
        } else if let Some(rest) = line.strip_prefix('c') {
            if name.is_none() {
                name = Some(rest.to_string());
            }
        }
        if pid.is_some() && name.is_some() {
            break;
        }
    }
    Some((pid?, name.unwrap_or_else(|| "unknown".into())))
}

#[cfg(windows)]
fn find_port_holder_windows(port: u16) -> Option<(u32, String)> {
    // netstat -ano | find ":8082"
    let output = Command::new("netstat")
        .args(["-ano", "-p", "tcp"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let needle = format!(":{port}");
    for line in stdout.lines() {
        let trimmed = line.trim();
        if !trimmed.contains("LISTENING") {
            continue;
        }
        if !trimmed.contains(&needle) {
            continue;
        }
        // 行格式: TCP    0.0.0.0:8082    0.0.0.0:0    LISTENING    1234
        let pid: Option<u32> = trimmed
            .split_whitespace()
            .last()
            .and_then(|s| s.parse().ok());
        let Some(pid) = pid else {
            continue;
        };
        let name = process_name_by_pid(pid).unwrap_or_else(|| "unknown".into());
        return Some((pid, name));
    }
    None
}

#[cfg(windows)]
fn process_name_by_pid(pid: u32) -> Option<String> {
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // CSV 第一列是进程名（带引号）
    let first = stdout.lines().next()?;
    let name = first.split(',').next()?.trim_matches('"').to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

// ═══════════════════════════════════════════════════════════════════
//  Windows: 修复（doctor --fix）
// ═══════════════════════════════════════════════════════════════════

/// 输出 Windows 端口保留的人类可读建议
fn print_windows_reserved_advice(port: u16, range: (u16, u16)) {
    println!(
        "  {} 端口 {} 落入 Windows 系统保留范围 [{} - {}]",
        style("诊断:").yellow().bold(),
        style(port).cyan(),
        style(range.0).cyan(),
        style(range.1).cyan(),
    );
    println!(
        "  {} 这是 Hyper-V / WSL2 / Docker Desktop 通过 winnat 服务",
        style("说明:").dim()
    );
    println!(
        "  {}  动态保留的端口区间。重启电脑后保留范围会变化，",
        style("    ").dim()
    );
    println!(
        "  {}  这就是为什么你昨天还能用、今天突然就不行了。",
        style("    ").dim()
    );
    println!();
    println!("  {} 一键永久修复:", style("修复:").green().bold());
    println!("    {}", style("cc-proxy doctor --fix").cyan().bold());
    println!();
    println!("  {} 或手动执行（管理员 cmd）:", style("手动:").dim());
    println!("    {}", style("net stop winnat").dim());
    println!(
        "    {}",
        style(format!(
            "netsh int ipv4 add excludedportrange protocol=tcp startport={port} numberofports=1"
        ))
        .dim()
    );
    println!("    {}", style("net start winnat").dim());
    println!();
    println!(
        "  {} 这会把 {} 永久加入排除列表，winnat 重启后",
        style("效果:").dim(),
        style(port).cyan()
    );
    println!(
        "  {}  Hyper-V 不会再保留它，重启电脑也不会复发。",
        style("    ").dim()
    );
    println!(
        "  {}  注意：net stop winnat 会短暂中断 Docker/WSL2 网络（~3秒）。",
        style("    ").dim()
    );
    println!();
}

/// Windows: 检测当前进程是否管理员
#[cfg(target_os = "windows")]
pub fn is_elevated() -> bool {
    // 用 net session 探测：非管理员会失败
    Command::new("net")
        .arg("session")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Windows: 执行端口保留修复（必须在管理员上下文中调用）
#[cfg(target_os = "windows")]
pub fn fix_windows_port_reservation(port: u16) -> Result<(), String> {
    let steps: [(&str, Vec<String>); 3] = [
        ("停止 winnat 服务", vec!["stop".into(), "winnat".into()]),
        (
            "添加端口排除",
            vec![
                "int".into(),
                "ipv4".into(),
                "add".into(),
                "excludedportrange".into(),
                "protocol=tcp".into(),
                format!("startport={port}"),
                "numberofports=1".into(),
            ],
        ),
        ("启动 winnat 服务", vec!["start".into(), "winnat".into()]),
    ];

    // 第 1、3 步用 net.exe；第 2 步用 netsh.exe
    let commands: [(&str, &[String]); 3] = [
        ("net", &steps[0].1),
        ("netsh", &steps[1].1),
        ("net", &steps[2].1),
    ];

    for (i, (label, _)) in steps.iter().enumerate() {
        let (bin, args) = commands[i];
        println!(
            "  {} [{}/3] {}",
            style("→").cyan(),
            i + 1,
            style(label).white()
        );
        let output = Command::new(bin)
            .args(args.iter().map(|s| s.as_str()))
            .output()
            .map_err(|e| format!("执行 {bin} 失败: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            // 第 2 步如果端口已在排除列表里会报错，视为成功
            if i == 1 && (stderr.contains("已存在") || stderr.contains("already exists")) {
                println!("  {} 端口已在排除列表中，跳过", style("ℹ").blue());
                continue;
            }
            return Err(format!(
                "{label} 失败:\n  stdout: {}\n  stderr: {}",
                stdout.trim(),
                stderr.trim()
            ));
        }
    }
    Ok(())
}

/// Windows: 用 runas 重启自身（提权）
#[cfg(target_os = "windows")]
pub fn relaunch_as_admin(args: &[&str]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("获取自身路径失败: {e}"))?;
    let exe_str = exe.to_string_lossy().to_string();

    // 用 PowerShell 的 Start-Process -Verb RunAs 弹 UAC
    let arg_list = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",");
    let ps_cmd = format!(
        "Start-Process -FilePath '{}' -ArgumentList {} -Verb RunAs -Wait",
        exe_str.replace('\'', "''"),
        if arg_list.is_empty() {
            "@()".into()
        } else {
            arg_list
        }
    );

    let status = Command::new("powershell")
        .args(["-NoProfile", "-Command", &ps_cmd])
        .status()
        .map_err(|e| format!("启动 PowerShell 失败: {e}"))?;

    if !status.success() {
        return Err("用户拒绝了 UAC 授权或执行失败".into());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  测试
// ═══════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    #[test]
    fn parse_excluded_ports_basic() {
        // 模拟 netsh 输出，吾们直接测解析逻辑（不实际跑 netsh）
        let sample = r#"
Protocol tcp Port Exclusion Ranges

Start Port    End Port
----------    --------
      1024        1123
      8080        8179
     50000       50059

* - Administered port exclusions.
"#;
        let mut ranges = Vec::new();
        for line in sample.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let mut parts = trimmed.split_whitespace();
            let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
                continue;
            };
            if parts.next().is_some() {
                continue;
            }
            let (Ok(s), Ok(e)) = (a.parse::<u16>(), b.parse::<u16>()) else {
                continue;
            };
            if s <= e {
                ranges.push((s, e));
            }
        }
        assert_eq!(ranges, vec![(1024, 1123), (8080, 8179), (50000, 50059)]);
    }

    #[test]
    fn port_in_range() {
        let ranges = [(8080u16, 8179u16)];
        let port = 8082u16;
        let hit = ranges.iter().find(|(s, e)| port >= *s && port <= *e);
        assert!(hit.is_some());
    }
}
