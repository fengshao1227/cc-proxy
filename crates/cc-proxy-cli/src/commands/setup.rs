use std::collections::HashMap;

use anyhow::{Context, Result};
use console::style;
use dialoguer::{Confirm, Input, Select};

use cc_proxy_core::config::ProxyConfig;

// ─── 内置模型列表 ───────────────────────────────────────────────────

const BUILTIN_MODELS: &[(&str, &str)] = &[
    ("gpt-5.4", "GPT-5.4 (最新旗舰)"),
    ("gpt-5.4-mini", "GPT-5.4 Mini (轻量快速)"),
    ("gpt-5.1", "GPT-5.1"),
    ("gpt-5.2", "GPT-5.2"),
    ("gpt-5", "GPT-5"),
    ("gpt-4o", "GPT-4o"),
    ("gpt-4o-mini", "GPT-4o Mini"),
    ("deepseek-chat", "DeepSeek Chat"),
    ("deepseek-reasoner", "DeepSeek Reasoner"),
];

const REASONING_LEVELS: &[(&str, &str)] = &[
    ("none", "关闭"),
    ("low", "低 — 简单任务"),
    ("medium", "中 — 日常编码"),
    ("high", "高 — 复杂调试"),
    ("xhigh", "极高 — 深度推理"),
];

// ═══════════════════════════════════════════════════════════════════
//  Entry Point
// ═══════════════════════════════════════════════════════════════════

pub async fn run() -> Result<()> {
    print_banner();

    // Step 1: API 连接
    print_section("API 连接");

    let base_url: String = Input::new()
        .with_prompt("  API Base URL")
        .default("https://api.openai.com/v1".to_string())
        .interact_text()
        .context("输入 URL 失败")?;

    let api_key: String = Input::new()
        .with_prompt("  API Key")
        .interact_text()
        .context("输入 Key 失败")?;

    if api_key.trim().is_empty() {
        anyhow::bail!("API Key 不能为空");
    }

    // Step 2: 模型配置
    print_section("模型配置");
    println!(
        "  {}",
        style("Claude Code 使用三个模型级别，你可以分别指定映射:").dim()
    );
    println!(
        "    {} → opus (大模型)    {} → sonnet (中模型)    {} → haiku (小模型)",
        style("BIG").green(),
        style("MIDDLE").yellow(),
        style("SMALL").cyan()
    );
    println!();

    let big_model = select_model("BIG (opus 映射)", "gpt-5.4")?;
    let big_reasoning = select_reasoning(&format!("BIG [{}]", big_model))?;

    let middle_model = select_model("MIDDLE (sonnet 映射)", "gpt-5.4")?;
    let middle_reasoning = select_reasoning(&format!("MIDDLE [{}]", middle_model))?;

    let small_model = select_model("SMALL (haiku 映射)", "gpt-5.4-mini")?;
    let small_reasoning = select_reasoning(&format!("SMALL [{}]", small_model))?;

    // Step 3: 服务设置
    print_section("服务设置");
    let port: u16 = Input::new()
        .with_prompt("  代理端口")
        .default(8082u16)
        .interact_text()
        .context("输入端口失败")?;

    // 自动生成鉴权密钥
    let anthropic_key = generate_auth_key();
    println!(
        "  {} 已自动生成鉴权密钥: {}",
        style("🔑").cyan(),
        style(&anthropic_key).green()
    );

    // Build config
    let middle_opt = if middle_model == big_model {
        None
    } else {
        Some(middle_model.clone())
    };

    let config = ProxyConfig {
        openai_api_key: api_key,
        openai_base_url: base_url,
        big_model,
        middle_model: middle_opt,
        small_model,
        host: "0.0.0.0".to_string(),
        port,
        anthropic_api_key: Some(anthropic_key),
        azure_api_version: None,
        log_level: "info".to_string(),
        max_tokens_limit: 128000,
        min_tokens_limit: 100,
        request_timeout: 600,
        streaming_first_byte_timeout: 300,
        streaming_idle_timeout: 300,
        connect_timeout: 30,
        token_count_scale: 0.5,
        custom_headers: HashMap::new(),
        reasoning_effort: "none".to_string(),
        big_reasoning: if big_reasoning == "none" {
            None
        } else {
            Some(big_reasoning)
        },
        middle_reasoning: if middle_reasoning == "none" {
            None
        } else {
            Some(middle_reasoning)
        },
        small_reasoning: if small_reasoning == "none" {
            None
        } else {
            Some(small_reasoning)
        },
    };

    // Save
    let path = ProxyConfig::default_config_path();
    config.save_to_file(&path).context("保存配置失败")?;

    // Summary
    print_summary(&config, &path);

    // Offer start
    let start_now = Confirm::new()
        .with_prompt(format!("  {}", style("现在启动代理？").bold()))
        .default(true)
        .interact()
        .context("确认启动失败")?;

    if start_now {
        println!();
        cc_proxy_core::server::serve(config).await?;
    } else {
        println!();
        println!(
            "  配置已保存。运行 {} 启动代理",
            style("cc-proxy start").bold().green()
        );
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════
//  UI Components
// ═══════════════════════════════════════════════════════════════════

fn print_banner() {
    let c = |s: &str| style(s).cyan().to_string();
    let d = |s: &str| style(s).dim().to_string();

    println!();
    println!(
        "  {}",
        c("┌─────────────────────────────────────────────────────┐")
    );
    println!(
        "  {}                                                     {}",
        c("│"),
        c("│")
    );
    println!(
        "  {}              {}              {}",
        c("│"),
        style("cc-proxy").bold().white(),
        c("│")
    );
    println!(
        "  {}        {}        {}",
        c("│"),
        d("Claude Code ↔ Any LLM Provider"),
        c("│")
    );
    println!(
        "  {}                                                     {}",
        c("│"),
        c("│")
    );
    println!(
        "  {}        {}   |   {}   |   {}            {}",
        c("│"),
        style(format!("v{}", env!("CARGO_PKG_VERSION"))).green(),
        style("Rust").yellow(),
        d("6.4MB"),
        c("│")
    );
    println!(
        "  {}                                                     {}",
        c("│"),
        c("│")
    );
    println!(
        "  {}",
        c("└─────────────────────────────────────────────────────┘")
    );
    println!();
}

fn print_section(title: &str) {
    println!();
    println!(
        "  {}── {} ──{}",
        style("─────").dim(),
        style(title).bold().cyan(),
        style("─────").dim()
    );
    println!();
}

/// Select a model from presets or custom input
fn select_model(label: &str, default: &str) -> Result<String> {
    let mut items: Vec<String> = BUILTIN_MODELS
        .iter()
        .map(|(id, desc)| format!("{} — {}", style(id).green(), style(desc).dim()))
        .collect();
    items.push(format!("{}", style("自定义模型 (手动输入)").yellow()));

    let default_idx = BUILTIN_MODELS
        .iter()
        .position(|(id, _)| *id == default)
        .unwrap_or(0);

    let idx = Select::new()
        .with_prompt(format!("  {}", style(label).bold()))
        .items(&items)
        .default(default_idx)
        .interact()
        .context("选择模型失败")?;

    if idx < BUILTIN_MODELS.len() {
        Ok(BUILTIN_MODELS[idx].0.to_string())
    } else {
        let model: String = Input::new()
            .with_prompt("  输入模型名称")
            .interact_text()
            .context("输入失败")?;
        Ok(model)
    }
}

/// Select reasoning effort for a specific model tier
fn select_reasoning(label: &str) -> Result<String> {
    let items: Vec<String> = REASONING_LEVELS
        .iter()
        .map(|(id, desc)| format!("{} — {}", style(id).cyan(), desc))
        .collect();

    let idx = Select::new()
        .with_prompt(format!("  {} 思考强度", style(label).bold()))
        .items(&items)
        .default(0)
        .interact()
        .context("选择思考强度失败")?;

    Ok(REASONING_LEVELS[idx].0.to_string())
}

fn mask_key(key: &str) -> String {
    if !key.is_ascii() || key.len() <= 8 {
        return "*".repeat(key.len().min(8));
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

fn print_summary(config: &ProxyConfig, path: &std::path::Path) {
    print_section("配置摘要");

    let r = |s: &str| {
        if s == "none" {
            style("关闭").dim().to_string()
        } else {
            style(s).cyan().to_string()
        }
    };

    println!(
        "  {} {}",
        style("Base URL:").dim(),
        style(&config.openai_base_url).white()
    );
    println!(
        "  {} {}",
        style("API Key: ").dim(),
        style(mask_key(&config.openai_api_key)).dim()
    );
    println!();
    println!(
        "  {} {:<16} reasoning: {}",
        style("BIG   (opus):  ").dim(),
        style(&config.big_model).green(),
        r(config.big_reasoning.as_deref().unwrap_or("none"))
    );
    println!(
        "  {} {:<16} reasoning: {}",
        style("MIDDLE(sonnet):").dim(),
        style(config.effective_middle_model()).yellow(),
        r(config.middle_reasoning.as_deref().unwrap_or("none"))
    );
    println!(
        "  {} {:<16} reasoning: {}",
        style("SMALL (haiku): ").dim(),
        style(&config.small_model).cyan(),
        r(config.small_reasoning.as_deref().unwrap_or("none"))
    );
    println!();
    println!(
        "  {} {}:{}",
        style("Server:").dim(),
        config.host,
        style(config.port).white()
    );
    if let Some(ref key) = config.anthropic_api_key {
        println!("  {} {}", style("鉴权密钥:").dim(), style(key).green());
    }
    println!();
    println!(
        "  {} {}",
        style("✔").green().bold(),
        style(format!("配置已保存: {}", path.display())).dim()
    );
}

/// Generate a random auth key
fn generate_auth_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:032x}", ts)
}
