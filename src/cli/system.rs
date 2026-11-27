//! Auth, diagnostics, daemon, HTTP/MCP server, and self-update CLI handlers.

use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use crate::{
    auth::{
        diagnose_access_token_scopes, read_external_token_file, AuthManager, LoginOptions,
    },
    config::{self, AppConfig, Persona},
    graph::GraphClient,
    img_preview,
    safe_text::sanitize_terminal_inline,
    serve::{self, ServeOptions},
    updater,
};

use super::{
    args::DaemonAction,
    data::init_runtime,
    format::{auth_source_label, print_daemon_status},
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_login(
    config: &AppConfig,
    client_id: Option<String>,
    tenant_id: Option<String>,
    device_code: bool,
    client_secret: Option<String>,
    access_token: Option<String>,
    token_file: Option<PathBuf>,
    port: Option<u16>,
    as_json: bool,
) -> Result<()> {
    let mut cfg = config.clone();
    if let Some(tf) = token_file {
        cfg.token_file = tf.to_string_lossy().into_owned();
        let tok = read_external_token_file(&tf, &cfg)?;
        // Only this key: `cfg` also holds whatever the environment set for this run.
        config::persist_config_keys(&[(
            "token_file",
            toml::Value::String(cfg.token_file.clone()),
        )])?;
        if as_json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "auth_mode": tok.auth_mode,
                    "token_file": cfg.token_file,
                    "user_id": tok.user_id,
                    "user_email": tok.user_email,
                }))?
            );
        } else {
            println!("Configured external token file: {}", tf.display());
        }
        return Ok(());
    }
    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth
        .login(&LoginOptions {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            port,
        })
        .await?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "auth_mode": tokens.auth_mode,
                "user_id": tokens.user_id,
                "display_name": tokens.user_display_name,
                "email": tokens.user_email,
                "expires_at": tokens.expires_at,
            }))?
        );
    } else {
        println!(
            "Signed in as {} ({})",
            sanitize_terminal_inline(
                tokens
                    .user_display_name
                    .as_deref()
                    .or(tokens.user_email.as_deref())
                    .unwrap_or("Microsoft 365 User")
            ),
            tokens.auth_mode
        );
    }
    Ok(())
}

pub(super) async fn cmd_logout(config: AppConfig, as_json: bool) -> Result<()> {
    let auth = AuthManager::new(config)?;
    auth.logout().await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Signed out and removed stored credentials.");
    }
    Ok(())
}

pub(super) async fn cmd_whoami(config: &AppConfig, persona: Persona, as_json: bool) -> Result<()> {
    let auth = AuthManager::new(config.clone())?;
    let tokens = auth.current_tokens().await;
    let source = auth_source_label(tokens.as_ref(), config);
    let Some(tok) = tokens else {
        if as_json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "authenticated": false,
                    "persona": persona.as_str(),
                    "auth_source": source,
                }))?
            );
        } else {
            println!("Not signed in (persona: {}). Run 'sharepoint login'.", persona.display_name());
        }
        return Ok(());
    };
    let diag = diagnose_access_token_scopes(&tok.access_token);
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "authenticated": true,
                "persona": persona.as_str(),
                "user_id": tok.user_id,
                "display_name": tok.user_display_name,
                "email": tok.user_email,
                "tenant_id": tok.tenant_id,
                "client_id": tok.client_id,
                "auth_mode": tok.auth_mode,
                "auth_source": source,
                "expires_at": tok.expires_at,
                "scope_diagnostics": diag,
            }))?
        );
    } else {
        println!("Persona:  {}", persona.display_name());
        println!(
            "User:     {} ({})",
            sanitize_terminal_inline(tok.user_display_name.as_deref().unwrap_or("Unknown")),
            sanitize_terminal_inline(tok.user_email.as_deref().unwrap_or("-"))
        );
        println!("Auth:     {} ({})", tok.auth_mode, sanitize_terminal_inline(&source));
        if !diag.warnings.is_empty() {
            for w in &diag.warnings {
                println!("Warning:  {}", sanitize_terminal_inline(w));
            }
        }
    }
    Ok(())
}

pub(super) async fn cmd_doctor(config: &AppConfig, persona: Persona, online: bool, as_json: bool) -> Result<()> {
    let auth = AuthManager::new(config.clone())?;
    let tokens = auth.current_tokens().await;
    let user_id = tokens.as_ref().and_then(|t| t.user_id.clone());
    let caps = img_preview::detect_graphics_caps(config.image_protocol);
    let mut online_probe = json!(null);
    if online {
        if let Ok(graph) = GraphClient::new(config.clone(), auth.clone()) {
            let (me_res, drive_res, sites_res) = tokio::join!(
                graph.get_me(),
                graph.get_my_drive(),
                graph.list_sites(Some("*"), 5),
            );
            online_probe = json!({
                "me": me_res.is_ok(),
                "onedrive": drive_res.is_ok(),
                "sharepoint_sites": sites_res.is_ok(),
            });
        }
    }
    let info = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "persona": persona.as_str(),
        "config_file": config::config_file_path(),
        "db_path": config::db_path(user_id.as_deref()),
        "download_dir": config.resolved_download_dir(),
        "graph_base_url": config.graph_base_url,
        "serve_port_default": config::DEFAULT_SERVE_PORT,
        "authenticated": tokens.is_some(),
        "auth_source": auth_source_label(tokens.as_ref(), config),
        "image_protocol": caps.protocol.as_str(),
        "online_probe": online_probe,
    });
    if as_json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        println!("@bitefoundry/sharepoint v{} ({})", env!("CARGO_PKG_VERSION"), persona.display_name());
        println!("Config:         {}", config::config_file_path().display());
        println!("SQLite DB:      {}", config::db_path(user_id.as_deref()).display());
        println!("Download dir:   {}", config.resolved_download_dir().display());
        println!("Graph URL:      {}", config.graph_base_url);
        println!("Authenticated:  {}", tokens.is_some());
        println!("Image protocol: {}", caps.protocol.label());
        if online {
            println!("Online probe:   {online_probe}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_daemon(config: AppConfig, action: DaemonAction) -> Result<()> {
    match action {
        DaemonAction::Start { json: as_json } => {
            let status = crate::daemon::ensure_daemon_started(&config).await?;
            print_daemon_status(&status, as_json)
        }
        DaemonAction::Status { json: as_json } => {
            let status = crate::daemon::query_daemon_status(&config).await?;
            print_daemon_status(&status, as_json)
        }
        DaemonAction::Stop { json: as_json } => {
            let stopped = crate::daemon::stop_daemon(&config).await?;
            if as_json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "ok": true, "stopped": stopped }))?
                );
            } else if stopped {
                println!("SharePoint background sync daemon stopped.");
            } else {
                println!("Daemon was not running.");
            }
            Ok(())
        }
        DaemonAction::Restart { json: as_json } => {
            let _ = crate::daemon::stop_daemon(&config).await;
            let status = crate::daemon::ensure_daemon_started(&config).await?;
            print_daemon_status(&status, as_json)
        }
        DaemonAction::Run => crate::daemon::run_daemon_foreground(config).await,
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_serve(
    config: AppConfig,
    bind: String,
    port: u16,
    api_key: Option<String>,
    allow_keyless: bool,
    allow_local_paths: bool,
    allowed_hosts: Vec<String>,
    cors_origins: Vec<String>,
) -> Result<()> {
    let (_, graph, storage) = init_runtime(&config, false).await?;
    serve::run(
        config,
        graph,
        storage,
        ServeOptions {
            bind,
            port,
            api_key,
            allow_keyless,
            allow_local_paths,
            allowed_hosts,
            cors_origins,
        },
    )
    .await
}

pub(super) async fn cmd_mcp(config: AppConfig) -> Result<()> {
    let (_, graph, storage) = init_runtime(&config, true).await?;
    crate::mcp::run(config, graph, storage).await
}

pub(super) async fn cmd_update(
    config: &AppConfig,
    check: bool,
    force: bool,
    repo: Option<String>,
    as_json: bool,
) -> Result<()> {
    let target_repo = repo.as_deref().unwrap_or(&config.github_repo);
    let res = updater::check_or_apply_update(target_repo, check, force).await?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&res)?);
    } else if res.updated {
        println!(
            "Updated sharepoint {} -> {} ({})",
            res.current_version,
            res.latest_version,
            res.binary_path.display()
        );
    } else if res.update_available {
        println!(
            "Update available: {} -> {} ({})",
            res.current_version, res.latest_version, res.release_url
        );
    } else {
        println!("Up to date (v{}).", res.current_version);
    }
    Ok(())
}
