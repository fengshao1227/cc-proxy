use anyhow::Result;
use cc_proxy_core::config::ProxyConfig;

use crate::daemon::read_pid;
use crate::diagnostics::{diagnose_port, print_diagnosis, PortDiagnosis};

/// 加载配置：优先 config.json，其次 env/.env
fn load_config() -> Result<ProxyConfig> {
    let config_path = ProxyConfig::default_config_path();
    if config_path.exists() {
        let config = ProxyConfig::load_from_file(&config_path)?;
        return Ok(config);
    }
    let config = ProxyConfig::load()?;
    Ok(config)
}

/// 检查代理运行状态并解析健康信息
async fn check_health(port: u16) -> Option<serde_json::Value> {
    let url = format!("http://localhost:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;

    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

/// 获取进程启动时间（macOS/Linux）
fn get_process_uptime(pid: u32) -> Option<String> {
    // 使用 ps 获取进程的 elapsed time
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "etime="])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let elapsed = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if elapsed.is_empty() {
        return None;
    }
    Some(elapsed)
}

pub async fn run() -> Result<()> {
    println!("=== cc-proxy 状态 ===\n");

    // 1. 加载并展示配置
    match load_config() {
        Ok(config) => {
            println!("[配置信息]");
            println!("  上游地址:   {}", config.openai_base_url);
            println!("  大模型:     {}", config.big_model);
            println!("  中模型:     {}", config.effective_middle_model());
            println!("  小模型:     {}", config.small_model);
            println!("  监听端口:   {}", config.port);
            println!("  监听地址:   {}", config.host);
            println!("  请求超时:   {}s", config.request_timeout);
            println!(
                "  客户端认证: {}",
                if config.anthropic_api_key.is_some() {
                    "已启用"
                } else {
                    "未启用"
                }
            );
            println!(
                "  API 密钥:   {}",
                if config.openai_api_key.len() > 8 {
                    format!(
                        "{}...{}",
                        &config.openai_api_key[..4],
                        &config.openai_api_key[config.openai_api_key.len() - 4..]
                    )
                } else {
                    "已配置".to_string()
                }
            );
            if !config.custom_headers.is_empty() {
                println!("  自定义头:   {} 个", config.custom_headers.len());
            }
            println!();

            // 2. 检查运行状态
            println!("[运行状态]");
            let port = config.port;

            match check_health(port).await {
                Some(health) => {
                    println!("  状态: 运行中");

                    // 尝试获取 PID 和运行时长
                    if let Some(pid) = read_pid() {
                        println!("  PID:  {pid}");
                        if let Some(uptime) = get_process_uptime(pid) {
                            println!("  运行时长: {uptime}");
                        }
                    }

                    // 从 health 响应提取额外信息
                    if let Some(ts) = health.get("timestamp").and_then(|v| v.as_str()) {
                        println!("  健康检查时间戳: {ts}");
                    }
                    let api_ok = health
                        .get("openai_api_configured")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    println!("  上游 API 已配置: {}", if api_ok { "是" } else { "否" });
                }
                None => {
                    println!("  状态: 未运行");

                    // 检查是否有残留 PID 文件
                    if let Some(pid) = read_pid() {
                        println!("  (发现残留 PID 文件，记录 PID: {pid}，进程可能已异常退出)");
                    }

                    // 主动诊断端口：未运行的真正原因可能是端口被系统保留
                    let diagnosis = diagnose_port(port);
                    match &diagnosis {
                        PortDiagnosis::WindowsReserved { .. } => {
                            println!();
                            print_diagnosis(&diagnosis);
                        }
                        PortDiagnosis::HeldByProcess {
                            pid: holder, name, ..
                        } => {
                            // 如果占用者就是自己的残留进程，不重复提示
                            if read_pid() != Some(*holder) {
                                println!();
                                println!("  ⚠ 端口 {port} 被其他进程占用: PID {holder} ({name})");
                                println!("    提示: 终止该进程或运行 `cc-proxy doctor` 查看详情");
                            } else {
                                println!("  提示: 使用 `cc-proxy start` 启动代理服务");
                            }
                        }
                        PortDiagnosis::Unknown { .. } => {
                            println!("  提示: 使用 `cc-proxy start` 启动代理服务");
                        }
                    }
                }
            }
        }
        Err(e) => {
            println!("[配置信息]");
            println!("  无法加载配置: {e}");
            println!("  提示: 请检查 ~/.cc-proxy/config.json 或设置 OPENAI_API_KEY 环境变量");
            println!();

            // 即使配置加载失败，仍尝试默认端口检查
            println!("[运行状态]");
            let port = 8082u16;
            match check_health(port).await {
                Some(_) => {
                    println!("  状态: 运行中 (端口 {port})");
                    if let Some(pid) = read_pid() {
                        println!("  PID:  {pid}");
                        if let Some(uptime) = get_process_uptime(pid) {
                            println!("  运行时长: {uptime}");
                        }
                    }
                }
                None => {
                    println!("  状态: 未运行");
                }
            }
        }
    }

    Ok(())
}
