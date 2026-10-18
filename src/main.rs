#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod agent;
mod app_commands;
mod chat_cli;
mod chat_commands;
mod chat_director;
mod chat_log;
mod chat_prompts;
mod chat_session;
mod chat_turn;
mod cli;
mod comfyui;
mod config;
mod context;
mod fileops_commands;
mod gpu;
mod headless;
mod llm;
mod memory;
mod paths;
mod persona;
mod recall;
mod rules;
mod ruleset;
mod sandbox;
mod searxng;
mod server;
mod tool_protocol;
mod tools;

use cli::Command;
use fileops_commands::AppState;
use simplelog::{
    ColorChoice, CombinedLogger, Config as LogConfig, ConfigBuilder as LogConfigBuilder,
    LevelFilter, TermLogger, TerminalMode, WriteLogger,
};
use std::fs;
use std::path::PathBuf;
use tauri::Manager;

/// Local wall-clock time in every log line instead of simplelog's UTC
/// default -- friendlier to read at a glance against "when did I actually
/// send that message" than doing the offset math by hand. Built once and
/// shared by both loggers below so the terminal and the file never disagree
/// with each other. Falls back to UTC (simplelog's own default) if `time`'s
/// local-offset detection fails, which it can on some platforms in a
/// multi-threaded process -- not fatal, just less convenient, and the only
/// place this gets reported is `eprintln!` since the logger itself isn't
/// initialized yet at this point.
fn log_config() -> LogConfig {
    let mut builder = LogConfigBuilder::new();
    if builder.set_time_offset_to_local().is_err() {
        eprintln!("could not determine the local time zone -- log timestamps will be UTC");
    }
    builder.build()
}

fn init_logging() {
    let log_dir = paths::app_log_dir();
    let _ = fs::create_dir_all(&log_dir);
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("app.log"));

    // Stderr, not Mixed: headless prints its result to stdout.
    let term = TermLogger::new(
        LevelFilter::Info,
        log_config(),
        TerminalMode::Stderr,
        ColorChoice::Auto,
    );

    match log_file {
        Ok(file) => {
            let file_logger = WriteLogger::new(LevelFilter::Debug, log_config(), file);
            let _ = CombinedLogger::init(vec![term, file_logger]);
            log::info!("logging to {}", log_dir.join("app.log").display());
        }
        Err(e) => {
            let _ = CombinedLogger::init(vec![term]);
            log::warn!("could not open log file in {}: {e}", log_dir.display());
        }
    }
}

fn print_help() {
    println!(
        "llm-assistant {} -- chat-driven local file assistant, sandboxed to a chosen folder",
        env!("CARGO_PKG_VERSION")
    );
    println!("\nUSAGE:");
    let usage: [(&str, &str); 10] = [
        ("llm-assistant", "Launch the GUI, no folder preloaded"),
        (
            "llm-assistant <folder>",
            "Launch the GUI with <folder> already open",
        ),
        (
            "llm-assistant <folder> <message...>",
            "Headless: run one turn against <folder>, print the result, and exit -- no GUI",
        ),
        (
            "llm-assistant <folder> --chat",
            "Interactive file-ops chat against <folder> -- no GUI, Ctrl+D/Ctrl+C to exit",
        ),
        (
            "llm-assistant --persona-chat [--persona <name>] [--session <id>]",
            "Chat mode's own CLI -- no folder, no shell commands, purely conversational",
        ),
        (
            "llm-assistant --list-personas",
            "List saved personas, for use with --persona",
        ),
        (
            "llm-assistant --list-sessions",
            "List chat mode sessions, for use with --session",
        ),
        (
            "llm-assistant --server [--bind <addr>] [--port <n>]",
            "Headless: serve chat mode only over HTTP -- no GUI, no folder, no file ops",
        ),
        ("llm-assistant --help | -h", "Show this help"),
        ("llm-assistant --version | -v", "Show the version"),
    ];
    for (cmd, desc) in usage {
        println!("    {cmd:<50} {desc}");
    }
    println!("\nConfig, rules, and logs live under $XDG_CONFIG_HOME/llm-assistant");
    println!("(usually ~/.config/llm-assistant):");
    let files: [(&str, &str); 5] = [
        (
            "config.toml",
            "endpoint, model, temperature, granted paths, ...",
        ),
        (
            "rules.md",
            "the working rules read before your system prompt",
        ),
        (
            "server.json",
            "--server's bind/port, password (required off-localhost) and allowed_hosts",
        ),
        ("logs/app.log", "internal debug log"),
        (
            "logs/chat-*.log",
            "mirror of the GUI conversation, one per launch, up to 5 kept",
        ),
    ];
    for (name, desc) in files {
        println!("    {name:<18} {desc}");
    }
}

fn fail_usage(msg: &str) -> ! {
    eprintln!("llm-assistant: {msg}");
    std::process::exit(2);
}

fn require_dir(folder: &std::path::Path) {
    if !folder.is_dir() {
        fail_usage(&format!("'{}' is not a directory", folder.display()));
    }
}

fn main() {
    // WebKitGTK's DMABUF renderer draws a blank white window on many
    // driver/compositor combinations. It bites the AppImage hardest, which
    // ships its own WebKitGTK, so a working host copy doesn't save it. Set
    // before any GTK/WebKit init; an explicit value from the environment wins.
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match cli::parse(&args) {
        Ok(c) => c,
        Err(msg) => fail_usage(&msg),
    };
    match &command {
        Command::Help => {
            print_help();
            return;
        }
        Command::Version => {
            println!("LLM Assistant {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Command::Gui {
            folder: Some(folder),
        }
        | Command::Headless { folder, .. }
        | Command::FolderRepl { folder } => require_dir(folder),
        _ => {}
    }

    init_logging();
    log::info!(
        "LLM Assistant starting, version={} build={}, config dir = {}",
        env!("CARGO_PKG_VERSION"),
        env!("BUILD_HASH"),
        paths::app_config_dir().display()
    );

    // Folder-less modes run before memory init, which is noise for them.
    let (cli_root, headless_job) = match command {
        Command::Help | Command::Version => unreachable!(),
        Command::ListPersonas => {
            chat_cli::list_personas();
            return;
        }
        Command::ListSessions => {
            chat_cli::list_sessions();
            return;
        }
        Command::PersonaChat { persona, session } => {
            log::info!("persona chat CLI: persona={persona:?} session={session:?}");
            chat_cli::run(chat_cli::Options {
                persona,
                session_id: session,
            });
        }
        // `--bind`/`--port` are per-run overrides, never written back to
        // `server.json`; the password is only ever read from that file (see
        // `server::ServerConfig`).
        Command::Server { bind, port } => {
            // Never fall back to defaults on a bad server.json: that would
            // silently drop the configured password.
            let cfg = match server::load_or_init() {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("llm-assistant: cannot load server config: {e}");
                    std::process::exit(1);
                }
            };
            let bind = bind.unwrap_or(cfg.bind);
            let port = port.unwrap_or(cfg.port);
            let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
            if let Err(e) = rt.block_on(server::run(bind, port, cfg.password)) {
                log::error!("web server exited with error: {e}");
                std::process::exit(1);
            }
            return;
        }
        Command::Headless { folder, message } => (None, Some((folder, Some(message)))),
        Command::FolderRepl { folder } => (None, Some((folder, None))),
        Command::Gui { folder } => (folder, None),
    };

    // Before the headless dispatch: a headless run is a session too. Not
    // gated on `memory_enabled` -- config is hot-reloaded, so the setting can
    // come on mid-session, and without a session dir the writes would fall
    // back to a shared one. Costs an empty directory when it stays off.
    match memory::init() {
        Ok(path) => log::info!("session memory: {}", path.display()),
        Err(e) => log::warn!("failed to start session memory: {e}"),
    }

    match headless_job {
        Some((root, Some(message))) => {
            log::info!("headless mode: root={} message={message:?}", root.display());
            headless::run(root, message);
        }
        Some((root, None)) => {
            log::info!("interactive chat mode: root={}", root.display());
            headless::run_chat(root);
        }
        None => {}
    }

    match chat_log::init() {
        Ok(path) => log::info!("chat log for this session: {}", path.display()),
        Err(e) => log::warn!("failed to start a new chat log: {e}"),
    }

    if let Some(root) = &cli_root {
        log::info!("preloaded root from CLI argument: {}", root.display());
    }
    let cli_root: Option<PathBuf> = cli_root;

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // `bundle.icon` in tauri.conf.json only reaches the window/taskbar
            // icon once the app is actually installed via .deb/AppImage (it's
            // baked into the desktop entry Linux reads the icon from) -- a
            // plain dev binary run directly has no such entry, so the window
            // manager falls back to a generic default. Setting it here at
            // runtime works unconditionally, dev binary or installed package
            // alike, since it talks to the windowing backend directly rather
            // than relying on desktop-entry registration.
            if let Some(window) = app.get_webview_window("main") {
                match tauri::image::Image::from_bytes(include_bytes!("../icons/128x128.png")) {
                    Ok(icon) => {
                        if let Err(e) = window.set_icon(icon) {
                            log::warn!("failed to set window icon: {e}");
                        }
                    }
                    Err(e) => log::warn!("failed to decode window icon: {e}"),
                }
            }
            Ok(())
        })
        .manage(AppState::new(cli_root))
        .invoke_handler(tauri::generate_handler![
            fileops_commands::pick_and_set_root,
            fileops_commands::pick_granted_path,
            fileops_commands::get_current_root,
            fileops_commands::unmount_root,
            app_commands::load_config,
            app_commands::save_config,
            chat_commands::get_comfyui_config,
            chat_commands::save_comfyui_config,
            chat_commands::get_searxng_config,
            chat_commands::save_searxng_config,
            chat_commands::test_searxng_search,
            chat_commands::pick_comfyui_output_dir,
            app_commands::default_system_prompt,
            app_commands::test_connection,
            app_commands::app_version,
            app_commands::app_build_hash,
            fileops_commands::load_general_rules,
            fileops_commands::save_general_rules,
            fileops_commands::default_general_rules,
            fileops_commands::load_command_rules,
            fileops_commands::save_command_rules,
            fileops_commands::default_command_rules,
            fileops_commands::add_granted_path,
            fileops_commands::remove_granted_path,
            fileops_commands::add_auto_approve,
            fileops_commands::remove_auto_approve,
            fileops_commands::agent_send,
            fileops_commands::agent_respond,
            fileops_commands::agent_stop,
            fileops_commands::agent_reset,
            fileops_commands::append_chat_log,
            chat_commands::list_personas,
            chat_commands::pick_persona_file,
            chat_commands::import_persona,
            chat_commands::save_new_persona,
            chat_commands::delete_persona,
            chat_commands::get_persona_content,
            chat_commands::update_persona,
            chat_commands::list_rulesets,
            chat_commands::get_ruleset_content,
            chat_commands::update_ruleset,
            chat_commands::get_ruleset_example,
            chat_commands::list_chat_sessions,
            chat_commands::create_chat_session,
            chat_commands::load_chat_session,
            chat_commands::rename_chat_session,
            chat_commands::delete_chat_session,
            chat_commands::get_chat_state,
            chat_commands::get_chat_raw_state,
            chat_commands::send_chat_message_streaming,
            chat_commands::stop_chat_generation,
            chat_commands::run_chat_followup,
            chat_commands::generate_chat_image,
            chat_commands::run_chat_web_search,
            chat_commands::test_comfyui_generation,
            chat_commands::read_generated_image,
            chat_commands::save_generated_image_as,
            chat_commands::test_vision_support,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
