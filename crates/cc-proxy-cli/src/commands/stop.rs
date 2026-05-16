use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::daemon::{is_process_alive, read_pid, remove_pid_file};

/// 向进程发送信号
fn send_signal(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .args([signal, &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 通过端口查找 cc-proxy 进程 PID
fn find_pid_by_port(port: u16) -> Option<u32> {
    // macOS: lsof -ti tcp:{port}
    // Linux: lsof -ti tcp:{port} 或 ss + 解析
    let output = Command::new("lsof")
        .args(["-ti", &format!("tcp:{port}")])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // 取第一个 PID
    stdout
        .lines()
        .next()
        .and_then(|line| line.trim().parse::<u32>().ok())
}

/// 加载配置中的端口号（用于端口查找兜底）
fn load_configured_port() -> u16 {
    let config_path = cc_proxy_core::config::ProxyConfig::default_config_path();
    if let Ok(config) = cc_proxy_core::config::ProxyConfig::load_from_file(&config_path) {
        return config.port;
    }
    if let Ok(config) = cc_proxy_core::config::ProxyConfig::load() {
        return config.port;
    }
    8082
}

pub fn run() -> Result<()> {
    // 1. 从 PID 文件获取进程 ID
    let pid = match read_pid() {
        Some(pid) => {
            if is_process_alive(pid) {
                println!("找到运行中的 cc-proxy 进程 (PID: {pid})");
                pid
            } else {
                println!("PID 文件存在但进程 {pid} 已不存在，清理残留文件...");
                remove_pid_file();
                // 兜底：尝试通过端口查找
                let port = load_configured_port();
                match find_pid_by_port(port) {
                    Some(p) => {
                        println!("通过端口 {port} 找到进程 (PID: {p})");
                        p
                    }
                    None => {
                        println!("cc-proxy 未在运行。");
                        return Ok(());
                    }
                }
            }
        }
        None => {
            // 无 PID 文件，尝试通过端口查找
            let port = load_configured_port();
            println!("未找到 PID 文件，尝试通过端口 {port} 查找进程...");
            match find_pid_by_port(port) {
                Some(pid) => {
                    println!("通过端口 {port} 找到进程 (PID: {pid})");
                    pid
                }
                None => {
                    println!("cc-proxy 未在运行。");
                    return Ok(());
                }
            }
        }
    };

    // 2. 发送 SIGTERM，优雅关闭
    println!("发送 SIGTERM 到进程 {pid}...");
    if !send_signal(pid, "-TERM") {
        bail!("无法向进程 {pid} 发送 SIGTERM，可能权限不足");
    }

    // 3. 等待进程退出，最多 5 秒
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if !is_process_alive(pid) {
            println!("cc-proxy 已优雅停止。");
            remove_pid_file();
            return Ok(());
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    // 4. 超时，升级为 SIGKILL
    println!("进程未在 5 秒内退出，发送 SIGKILL...");
    if !send_signal(pid, "-KILL") {
        bail!("无法向进程 {pid} 发送 SIGKILL");
    }

    // 等一下确认
    thread::sleep(Duration::from_millis(500));
    if is_process_alive(pid) {
        bail!("进程 {pid} 在 SIGKILL 后仍然存活，请手动处理");
    }

    println!("cc-proxy 已强制终止。");
    remove_pid_file();

    Ok(())
}
