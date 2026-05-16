//! Read/write Claude Code's ~/.claude/settings.json to auto-configure proxy env vars.

use std::path::PathBuf;

use anyhow::{Context, Result};
use console::style;

/// Path to Claude Code settings
fn settings_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".claude")
        .join("settings.json")
}

/// Check if Claude Code settings.json exists
pub fn claude_code_installed() -> bool {
    settings_path().exists()
}

/// Check if Claude Code is already configured to use cc-proxy
pub fn is_configured() -> bool {
    let path = settings_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    val.get("env")
        .and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(|v| v.as_str())
        .is_some_and(|url| url.contains("localhost"))
}

/// Configure Claude Code to use cc-proxy
pub fn configure(port: u16, auth_key: &str) -> Result<()> {
    let path = settings_path();

    // Read existing settings or create new
    let mut settings: serde_json::Value = if path.exists() {
        let content =
            std::fs::read_to_string(&path).context("读取 Claude Code settings.json 失败")?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        // Create .claude dir if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).context("创建 .claude 目录失败")?;
        }
        serde_json::json!({})
    };

    // Ensure env object exists
    if settings.get("env").is_none() {
        settings["env"] = serde_json::json!({});
    }

    let env = settings["env"].as_object_mut().unwrap();

    // Set proxy env vars
    env.insert(
        "ANTHROPIC_BASE_URL".into(),
        serde_json::json!(format!("http://localhost:{port}")),
    );
    env.insert("ANTHROPIC_API_KEY".into(), serde_json::json!(auth_key));
    // Clear auth token to force API key mode (avoids auth conflict)
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), serde_json::json!(""));

    // Write back
    let content = serde_json::to_string_pretty(&settings).context("序列化 settings.json 失败")?;
    std::fs::write(&path, content).context("写入 settings.json 失败")?;

    Ok(())
}

/// Remove cc-proxy env vars from Claude Code settings
pub fn unconfigure() -> Result<()> {
    let path = settings_path();
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&path).context("读取 settings.json 失败")?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(env) = settings.get_mut("env").and_then(|e| e.as_object_mut()) {
        env.remove("ANTHROPIC_BASE_URL");
        env.remove("ANTHROPIC_API_KEY");
        env.remove("ANTHROPIC_AUTH_TOKEN");
    }

    let content = serde_json::to_string_pretty(&settings)?;
    std::fs::write(&path, content)?;

    Ok(())
}

/// Print current Claude Code proxy config status
pub fn print_status() {
    if !claude_code_installed() {
        println!("  {} Claude Code settings.json 不存在", style("⚠").yellow());
        return;
    }

    if is_configured() {
        println!(
            "  {} Claude Code 已配置为使用 cc-proxy",
            style("✔").green().bold()
        );
    } else {
        println!("  {} Claude Code 尚未配置使用 cc-proxy", style("○").dim());
    }
}
