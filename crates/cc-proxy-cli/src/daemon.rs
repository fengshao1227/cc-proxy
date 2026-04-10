use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::diagnostics::{diagnose_port, print_diagnosis, PortDiagnosis};

/// PID file location: ~/.cc-proxy/proxy.pid
pub fn pid_file_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cc-proxy")
        .join("proxy.pid")
}

/// Write PID to file
pub fn write_pid(pid: u32) -> anyhow::Result<()> {
    let path = pid_file_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, pid.to_string())?;
    Ok(())
}

/// Read PID from file
pub fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Remove PID file
pub fn remove_pid_file() {
    let _ = fs::remove_file(pid_file_path());
}

/// Check if a process is alive
pub fn is_process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let pid = pid as libc::pid_t;
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        // On Windows, use tasklist to check if PID exists
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
}

/// 读取配置中的端口（用于健康检查）
fn configured_port() -> u16 {
    let path = cc_proxy_core::config::ProxyConfig::default_config_path();
    if let Ok(config) = cc_proxy_core::config::ProxyConfig::load_from_file(&path) {
        return config.port;
    }
    if let Ok(config) = cc_proxy_core::config::ProxyConfig::load() {
        return config.port;
    }
    8082
}

/// 同步健康检查：连 /health 端点
fn sync_health_check(port: u16) -> bool {
    use std::io::{Read, Write};
    use std::net::{TcpStream, ToSocketAddrs};

    let addr = match format!("127.0.0.1:{port}").to_socket_addrs() {
        Ok(mut it) => match it.next() {
            Some(a) => a,
            None => return false,
        },
        Err(_) => return false,
    };

    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(800)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));

    let req =
        format!("GET /health HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return false;
    }
    buf[..n].starts_with(b"HTTP/1.1 200") || buf[..n].starts_with(b"HTTP/1.0 200")
}

/// 读取日志文件尾部
fn read_log_tail(path: &Path, max_bytes: usize) -> String {
    let Ok(content) = fs::read_to_string(path) else {
        return String::new();
    };
    if content.len() <= max_bytes {
        return content;
    }
    let start = content.len() - max_bytes;
    // 对齐到下一个换行
    let offset = content[start..]
        .find('\n')
        .map(|i| start + i + 1)
        .unwrap_or(start);
    content[offset..].to_string()
}

/// Start the proxy in daemon mode by re-executing self
pub fn start_daemon() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let port = configured_port();

    // ── Pre-flight: 端口预检 ──
    // 在 spawn 之前先确认端口可 bind，否则立即输出诊断（含 doctor --fix 建议）
    // 避免 daemon 启动后"沉默失败"或误判（例如端口被其他 cc-proxy 实例占用时）
    let probe_addr = format!("0.0.0.0:{port}");
    match std::net::TcpListener::bind(&probe_addr) {
        Ok(listener) => drop(listener), // 立即释放
        Err(e) => {
            crate::diagnostics::print_bind_failure_advice(&probe_addr, port, &e);
            anyhow::bail!("端口 {port} 不可用，请先解决上述问题再启动");
        }
    }

    // Log to file instead of /dev/null
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".cc-proxy");
    fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join("proxy.log");
    let log_file = fs::File::create(&log_path)?;
    let err_file = log_file.try_clone()?;

    let child = Command::new(exe)
        .arg("start")
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(err_file)
        .spawn()?;

    let pid = child.id();
    write_pid(pid)?;

    // ── 启动后健康检查：最多等 3 秒 ──
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut healthy = false;
    let mut alive = true;
    while Instant::now() < deadline {
        alive = is_process_alive(pid);
        if !alive {
            break;
        }
        if sync_health_check(port) {
            healthy = true;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    if healthy {
        println!("🚀 cc-proxy 已在后台启动 (PID: {pid})");
        println!("   PID 文件: {}", pid_file_path().display());
        println!("   日志文件: {}", log_path.display());
        println!();
        println!("   停止: cc-proxy stop");
        println!("   状态: cc-proxy status");
        return Ok(());
    }

    // ── 启动失败路径：收集证据 + 主动诊断 ──
    println!("✗ cc-proxy 后台启动失败");
    println!();

    if !alive {
        println!("  子进程已退出（PID {pid} 不再存活）");
    } else {
        println!("  子进程 PID {pid} 仍在运行，但 /health 在 3 秒内无响应");
    }
    println!("  日志文件: {}", log_path.display());
    println!();

    // 读日志尾部
    let tail = read_log_tail(&log_path, 2048);
    let tail = tail.trim();
    if !tail.is_empty() {
        println!("  ── 日志尾部 ──");
        for line in tail.lines().rev().take(15).collect::<Vec<_>>().iter().rev() {
            println!("  {line}");
        }
        println!();
    }

    // 清理残留 PID（如果子进程已退出）
    if !alive {
        remove_pid_file();
    }

    // 主动诊断端口（子进程启动失败最常见就是 bind 失败）
    let diagnosis = diagnose_port(port);
    match diagnosis {
        PortDiagnosis::WindowsReserved { .. }
        | PortDiagnosis::HeldByProcess { .. }
        | PortDiagnosis::Unknown { .. } => {
            print_diagnosis(&diagnosis);
        }
    }

    anyhow::bail!("后台启动未能通过健康检查，请查看日志或运行 cc-proxy doctor");
}
