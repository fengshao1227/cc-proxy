use clap::{Parser, Subcommand};
use console::style;
use dialoguer::Select;

mod claude_config;
mod commands;
mod daemon;
mod diagnostics;

#[derive(Parser)]
#[command(name = "cc-proxy")]
#[command(about = "Claude Code Proxy — use any OpenAI-compatible API with Claude Code")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy server (foreground)
    Start {
        /// Run in daemon mode (background)
        #[arg(short, long)]
        daemon: bool,
    },
    /// Stop the background proxy
    Stop,
    /// Show proxy status and configuration
    Status,
    /// Interactive configuration wizard
    Setup,
    /// Test upstream API connection
    Test,
    /// Diagnose port / config / process issues (use --fix on Windows for one-click repair)
    Doctor {
        /// Auto-fix Windows port reservation (requires admin elevation via UAC)
        #[arg(long)]
        fix: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(cmd) => {
            let _ = dotenvy::dotenv();
            init_tracing();
            match cmd {
                Commands::Start { daemon } => {
                    if daemon {
                        daemon::start_daemon()?;
                    } else {
                        commands::start::run().await?;
                    }
                }
                Commands::Stop => commands::stop::run()?,
                Commands::Status => commands::status::run().await?,
                Commands::Setup => commands::setup::run().await?,
                Commands::Test => commands::test::run().await?,
                Commands::Doctor { fix } => commands::doctor::run(fix).await?,
            }
        }
        None => interactive_menu().await?,
    }

    Ok(())
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,hyper=warn,reqwest=warn".into()),
        )
        .init();
}

// ═══════════════════════════════════════════════════════════════════
//  Interactive Menu
// ═══════════════════════════════════════════════════════════════════

async fn interactive_menu() -> anyhow::Result<()> {
    print_banner();

    loop {
        let has_config = config_file_exists();
        let proxy_running = check_proxy_running().await;
        let cc_configured = claude_config::is_configured();

        // ── Status bar ──
        if has_config {
            let proxy_status = if proxy_running {
                style("● 代理运行中").green().to_string()
            } else {
                style("○ 代理未运行").dim().to_string()
            };
            let cc_status = if cc_configured {
                style("● Claude Code 已接入").green().to_string()
            } else {
                style("○ Claude Code 未接入").yellow().to_string()
            };
            println!("  {}    {}", proxy_status, cc_status);
        } else {
            println!("  {}", style("⚠ 尚未配置").yellow());
        }
        println!();

        // ── Build menu ──
        let mut items: Vec<String> = Vec::new();
        let mut actions: Vec<&str> = Vec::new();

        if has_config {
            // 启动/重启
            if proxy_running {
                items.push(mi("🔄", "重启代理", "停止后重新启动"));
                actions.push("restart");
            } else {
                items.push(mi("▶", "启动代理", "后台启动"));
                actions.push("start");
            }

            // Claude Code 一键接入/断开
            if cc_configured {
                items.push(mi(
                    "🔌",
                    "断开 Claude Code",
                    "从 settings.json 移除代理配置",
                ));
                actions.push("cc-disconnect");
            } else {
                items.push(mi("⚡", "接入 Claude Code", "一键写入 settings.json"));
                actions.push("cc-connect");
            }

            items.push(mi("🔑", "连接信息", "查看地址和密钥"));
            actions.push("info");

            items.push(mi(
                "📊",
                "查看状态",
                if proxy_running {
                    "运行中"
                } else {
                    "已停止"
                },
            ));
            actions.push("status");

            items.push(mi("🔗", "测试连接", "测试上游 API"));
            actions.push("test");

            items.push(mi("🩺", "体检修复", "诊断端口/进程/配置问题"));
            actions.push("doctor");

            if proxy_running {
                items.push(mi("⏹", "停止代理", ""));
                actions.push("stop");
            }
        }

        items.push(mi(
            "⚙",
            "配置向导",
            if has_config {
                "修改配置"
            } else {
                "首次使用请先配置"
            },
        ));
        actions.push("setup");

        items.push(mi("Q", "退出", ""));
        actions.push("quit");

        let default_idx = if !has_config { items.len() - 2 } else { 0 };

        let selection = Select::new()
            .with_prompt(format!("  {}", style("选择操作").bold()))
            .items(&items)
            .default(default_idx)
            .interact_opt()?;

        let Some(idx) = selection else { break };

        println!();

        match actions[idx] {
            "start" => {
                daemon::start_daemon()?;
                // 启动后自动接入 Claude Code
                auto_connect_claude_code();
            }
            "restart" => {
                println!("  {} 正在重启...", style("🔄").yellow());
                let _ = commands::stop::run();
                std::thread::sleep(std::time::Duration::from_secs(1));
                daemon::start_daemon()?;
                auto_connect_claude_code();
            }
            "cc-connect" => {
                connect_claude_code();
            }
            "cc-disconnect" => {
                disconnect_claude_code();
            }
            "info" => {
                print_connection_info();
            }
            "status" => {
                commands::status::run().await?;
                println!();
                claude_config::print_status();
            }
            "test" => {
                commands::test::run().await?;
            }
            "doctor" => {
                commands::doctor::run(false).await?;
                // 在菜单里发现问题时，询问是否立即修复（仅 Windows）
                #[cfg(target_os = "windows")]
                {
                    use dialoguer::Confirm;
                    let fix = Confirm::new()
                        .with_prompt("  是否立即运行 doctor --fix 一键修复？")
                        .default(true)
                        .interact()
                        .unwrap_or(false);
                    if fix {
                        commands::doctor::run(true).await?;
                    }
                }
            }
            "stop" => {
                commands::stop::run()?;
            }
            "setup" => {
                commands::setup::run().await?;
                continue;
            }
            "quit" => break,
            _ => break,
        }

        println!();
    }

    Ok(())
}

// ── Claude Code Integration ──

fn connect_claude_code() {
    let port = get_config_port();
    let auth_key = get_config_auth_key().unwrap_or_else(|| "any-value".into());

    match claude_config::configure(port, &auth_key) {
        Ok(()) => {
            println!(
                "  {} Claude Code 已配置为使用 cc-proxy",
                style("✔").green().bold()
            );
            println!("  {} 已写入 ~/.claude/settings.json", style("  ").dim());
            println!();
            println!(
                "  {} 直接运行 {} 即可使用代理",
                style("💡").cyan(),
                style("claude").green().bold()
            );
        }
        Err(e) => {
            println!("  {} 配置 Claude Code 失败: {e}", style("✗").red().bold());
        }
    }
}

fn disconnect_claude_code() {
    match claude_config::unconfigure() {
        Ok(()) => {
            println!(
                "  {} 已从 Claude Code 移除代理配置",
                style("✔").green().bold()
            );
            println!("  {} Claude Code 将恢复使用官方 API", style("  ").dim());
        }
        Err(e) => {
            println!("  {} 移除配置失败: {e}", style("✗").red().bold());
        }
    }
}

fn auto_connect_claude_code() {
    if !claude_config::claude_code_installed() {
        print_connection_info();
        return;
    }

    if claude_config::is_configured() {
        println!();
        println!(
            "  {} Claude Code 已接入，直接运行 {} 即可",
            style("✔").green().bold(),
            style("claude").green().bold()
        );
        return;
    }

    // 自动接入
    let port = get_config_port();
    let auth_key = get_config_auth_key().unwrap_or_else(|| "any-value".into());
    match claude_config::configure(port, &auth_key) {
        Ok(()) => {
            println!();
            println!(
                "  {} Claude Code 已自动接入，直接运行 {} 即可",
                style("⚡").cyan(),
                style("claude").green().bold()
            );
        }
        Err(_) => {
            print_connection_info();
        }
    }
}

// ── Helpers ──

fn mi(icon: &str, label: &str, desc: &str) -> String {
    if desc.is_empty() {
        format!("  {}  {}", icon, style(label).white())
    } else {
        format!(
            "  {}  {:<16} {}",
            icon,
            style(label).white(),
            style(format!("— {desc}")).dim()
        )
    }
}

fn config_file_exists() -> bool {
    cc_proxy_core::config::ProxyConfig::default_config_path().exists()
}

fn read_config_json() -> Option<serde_json::Value> {
    let path = cc_proxy_core::config::ProxyConfig::default_config_path();
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn get_config_port() -> u16 {
    read_config_json()
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .map(|p| p as u16)
        .unwrap_or(8082)
}

fn get_config_auth_key() -> Option<String> {
    read_config_json().and_then(|v| {
        v.get("anthropic_api_key")
            .and_then(|k| k.as_str())
            .map(String::from)
    })
}

async fn check_proxy_running() -> bool {
    let port = get_config_port();
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

fn print_connection_info() {
    let port = get_config_port();
    let auth_key = get_config_auth_key().unwrap_or_else(|| "any-value".into());

    println!(
        "  {}── {} ──{}",
        style("──────").dim(),
        style("连接信息").bold().cyan(),
        style("──────").dim()
    );
    println!();
    println!(
        "  {}  http://localhost:{}",
        style("代理地址:").dim(),
        style(port).green()
    );
    println!(
        "  {}  {}",
        style("鉴权密钥:").dim(),
        style(&auth_key).green()
    );
    println!();

    if claude_config::claude_code_installed() {
        claude_config::print_status();
        if !claude_config::is_configured() {
            println!(
                "  {} 选择菜单「接入 Claude Code」可一键配置",
                style("💡").cyan()
            );
        }
    } else {
        println!("  {} 手动启动:", style("使用方法:").dim());
        println!(
            "  {}",
            style(format!(
                "ANTHROPIC_BASE_URL=http://localhost:{port} ANTHROPIC_API_KEY=\"{auth_key}\" ANTHROPIC_AUTH_TOKEN=\"\" claude"
            ))
            .green()
        );
    }
    println!();
}

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
