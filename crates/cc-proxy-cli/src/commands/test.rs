use std::time::Instant;

use anyhow::Result;
use cc_proxy_core::config::ProxyConfig;

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

/// 检查代理是否在运行
async fn is_proxy_running(port: u16) -> bool {
    let url = format!("http://localhost:{port}/health");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap_or_default();

    client
        .get(&url)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// 通过代理的 /test-connection 端点测试
async fn test_via_proxy(port: u16) -> Result<()> {
    let url = format!("http://localhost:{port}/test-connection");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    println!("通过代理 (localhost:{port}) 的 /test-connection 端点测试...\n");

    let start = Instant::now();
    let resp = client.get(&url).send().await?;
    let latency = start.elapsed();

    let body: serde_json::Value = resp.json().await?;

    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if status == "success" {
        let model = body
            .get("model_used")
            .and_then(|v| v.as_str())
            .unwrap_or("未知");
        let resp_id = body
            .get("response_id")
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        println!("  连接测试成功！");
        println!("  使用模型:   {model}");
        println!("  响应 ID:    {resp_id}");
        println!("  延迟:       {:.0}ms", latency.as_millis());
    } else {
        let error = body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("未知错误");
        println!("  连接测试失败");
        println!("  错误信息: {error}");
        println!("  延迟:     {:.0}ms", latency.as_millis());
    }

    Ok(())
}

/// 直接向上游 API 发送测试请求
async fn test_upstream_direct(config: &ProxyConfig) -> Result<()> {
    let base_url = config.openai_base_url.trim_end_matches('/');
    let url = format!("{base_url}/chat/completions");
    let model = &config.small_model;

    println!("直接向上游 API 发送测试请求...");
    println!("  目标地址: {url}");
    println!("  测试模型: {model}\n");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let payload = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": "Hello" }
        ],
        "max_tokens": 5,
        "temperature": 0.0,
        "stream": false
    });

    let start = Instant::now();
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .bearer_auth(&config.openai_api_key)
        .json(&payload)
        .send()
        .await;
    let latency = start.elapsed();

    match resp {
        Ok(response) => {
            let status_code = response.status();
            if status_code.is_success() {
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                let resp_id = body.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                let reply = body
                    .pointer("/choices/0/message/content")
                    .and_then(|v| v.as_str())
                    .unwrap_or("-");

                println!("  连接测试成功！");
                println!("  使用模型:   {model}");
                println!("  响应 ID:    {resp_id}");
                println!("  模型回复:   {reply}");
                println!("  延迟:       {:.0}ms", latency.as_millis());
            } else {
                let body = response.text().await.unwrap_or_default();
                println!("  连接测试失败 (HTTP {})", status_code.as_u16());
                println!("  延迟:     {:.0}ms", latency.as_millis());
                println!("  响应详情:");

                // 尝试格式化 JSON 错误
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                    if let Some(err) = json.get("error") {
                        if let Some(msg) = err.get("message").and_then(|v| v.as_str()) {
                            println!("    错误信息: {msg}");
                        }
                        if let Some(t) = err.get("type").and_then(|v| v.as_str()) {
                            println!("    错误类型: {t}");
                        }
                    } else {
                        println!("    {json}");
                    }
                } else {
                    // 截断过长的原始响应
                    let display = if body.len() > 500 {
                        &body[..500]
                    } else {
                        &body
                    };
                    println!("    {display}");
                }
            }
        }
        Err(e) => {
            println!("  连接失败！");
            println!("  延迟: {:.0}ms", latency.as_millis());

            if e.is_timeout() {
                println!("  原因: 请求超时 (30s)");
            } else if e.is_connect() {
                println!("  原因: 无法连接到上游服务器");
                println!("  请检查 OPENAI_BASE_URL 是否正确: {base_url}");
            } else {
                println!("  原因: {e}");
            }
        }
    }

    Ok(())
}

pub async fn run() -> Result<()> {
    println!("=== cc-proxy 连接测试 ===\n");

    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            println!("无法加载配置: {e}");
            println!("请检查 ~/.cc-proxy/config.json 或设置 OPENAI_API_KEY 环境变量");
            return Ok(());
        }
    };

    // 优先通过代理测试（如果代理在运行）
    if is_proxy_running(config.port).await {
        println!("检测到 cc-proxy 正在运行 (端口 {})\n", config.port);
        test_via_proxy(config.port).await?;
    } else {
        println!("cc-proxy 未在运行，直接测试上游 API 连接\n");
        test_upstream_direct(&config).await?;
    }

    Ok(())
}
