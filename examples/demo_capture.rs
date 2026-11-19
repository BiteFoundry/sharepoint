//! Captures real 120x34 Ratatui TUI frames for `@shinick/sharepoint` using synthetic English demo
//! data and renders `assets/demo.gif` via the shared `render_tui_gif.py` script.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use sharepoint_cli::{
    auth::{AuthManager, StoredTokens},
    config::{AppConfig, DensityMode, Persona, ThemePreset},
    graph::GraphClient,
    img_preview::{TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        DriveInfo, DriveItem, DriveItemVersion, PinnedTarget, SharePointSite, UserProfile,
    },
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        app::{ActiveViewSource, AppState, Focus, UiMsg, UiState},
        overlays::{
            command_palette::CommandPalette,
            context_menu::{ContextMenu, ContextTarget},
            file_picker::{FilePickerEntry, FilePickerMode, FilePickerOverlay},
            links::LinkExtractorOverlay,
            settings::SettingsState,
            Overlay,
        },
        theme::Theme,
    },
};
use tokio::sync::{mpsc, watch};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;
const RENDER_SCRIPT: &str = r"C:\Users\user\.gemini\antigravity-cli\brain\f9e7f082-6ac8-4e73-9827-8af29ccd4ff4\scratch\render_tui_gif.py";
const PREVIEW_DIR: &str = r"C:\Users\user\.gemini\antigravity-cli\brain\f9e7f082-6ac8-4e73-9827-8af29ccd4ff4\scratch\gif-previews\sharepoint";

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = indexed_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (24, 26, 33),
        1 => (247, 118, 142),
        2 => (158, 206, 106),
        3 => (224, 175, 104),
        4 => (122, 162, 247),
        5 => (187, 154, 247),
        6 => (125, 207, 255),
        7 => (169, 177, 214),
        8 => (86, 95, 137),
        9 => (255, 140, 160),
        10 => (175, 224, 128),
        11 => (240, 200, 130),
        12 => (148, 184, 255),
        13 => (205, 176, 255),
        14 => (155, 222, 255),
        15 => (240, 244, 252),
        16..=231 => {
            let n = idx - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let to_v = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (to_v(r), to_v(g), to_v(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn modifier_flags(m: Modifier) -> u8 {
    let mut flags = 0u8;
    if m.contains(Modifier::BOLD) {
        flags |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        flags |= 2;
    }
    if m.contains(Modifier::DIM) {
        flags |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        flags |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        flags |= 16;
    }
    flags
}

/// Maps each Ratatui cell symbol to a single glyph supported by both CascadiaMono and Consolas Bold.
fn normalize_cell_char(sym: &str) -> char {
    let Some(ch) = sym.chars().next() else {
        return ' ';
    };
    match ch {
        '★' => '*',
        '◷' => '○',
        '⇄' => '»',
        '📁' => '▸',
        '📄' => '▪',
        '⚡' => '▸',
        other => other,
    }
}

fn buffer_to_rows(buf: &Buffer) -> Value {
    let mut rows = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<(Color, Color, u8)> = None;
        let mut skip_next = false;

        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let (ch, fg, bg, flags) = if skip_next || cell.skip {
                skip_next = false;
                let prev_bg = cur_style.map(|(_, b, _)| b).unwrap_or(cell.bg);
                (' ', cell.fg, prev_bg, 0)
            } else {
                let sym = cell.symbol();
                if unicode_width::UnicodeWidthStr::width(sym) >= 2 {
                    skip_next = true;
                }
                (
                    normalize_cell_char(sym),
                    cell.fg,
                    cell.bg,
                    modifier_flags(cell.modifier),
                )
            };
            let key = (fg, bg, flags);

            if cur_style == Some(key) {
                cur_text.push(ch);
            } else {
                if let Some((fg, bg, flags)) = cur_style.take() {
                    runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
                    cur_text.clear();
                }
                cur_text.push(ch);
                cur_style = Some(key);
            }
        }
        if let Some((fg, bg, flags)) = cur_style {
            runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
        }
        rows.push(Value::Array(runs));
    }
    Value::Array(rows)
}

fn capture_frame(
    terminal: &mut Terminal<TestBackend>,
    app: &mut AppState,
    ui: &mut UiState,
    duration_ms: u64,
    caption: &str,
    key_badge: &str,
) -> Result<Value> {
    terminal.clear()?;
    terminal.draw(|f| {
        ui::draw(f, app, ui);
    })?;
    let rows = buffer_to_rows(terminal.backend().buffer());
    Ok(json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    }))
}

fn onedrive_root_items() -> Vec<DriveItem> {
    vec![
        DriveItem {
            id: "od-fld-projects".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "Projects".to_string(),
            size: 0,
            is_folder: true,
            child_count: Some(8),
            mime_type: None,
            web_url: Some("https://acme.example/onedrive/Projects".to_string()),
            last_modified_at: Some("2026-11-19T16:10:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "od-fld-designs".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "Design Assets".to_string(),
            size: 0,
            is_folder: true,
            child_count: Some(14),
            mime_type: None,
            web_url: Some("https://acme.example/onedrive/Design-Assets".to_string()),
            last_modified_at: Some("2026-11-18T11:30:00Z".to_string()),
            last_modified_by: Some("Maya Patel".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "od-file-roadmap".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "q4-roadmap.md".to_string(),
            size: 4_820,
            is_folder: false,
            mime_type: Some("text/markdown".to_string()),
            web_url: Some("https://acme.example/onedrive/q4-roadmap.md".to_string()),
            last_modified_at: Some("2026-11-19T15:45:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            shared_scope: Some("Organization (view)".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "od-file-budget".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "infra-budget-2026.csv".to_string(),
            size: 28_410,
            is_folder: false,
            mime_type: Some("text/csv".to_string()),
            web_url: Some("https://acme.example/onedrive/infra-budget-2026.csv".to_string()),
            last_modified_at: Some("2026-11-17T09:20:00Z".to_string()),
            last_modified_by: Some("Jordan Lee".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "od-file-arch".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "topology-diagram.png".to_string(),
            size: 412_600,
            is_folder: false,
            mime_type: Some("image/png".to_string()),
            web_url: Some("https://acme.example/onedrive/topology-diagram.png".to_string()),
            last_modified_at: Some("2026-11-16T14:05:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "od-file-release".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "release-checklist.toml".to_string(),
            size: 2_190,
            is_folder: false,
            mime_type: Some("application/toml".to_string()),
            web_url: Some("https://acme.example/onedrive/release-checklist.toml".to_string()),
            last_modified_at: Some("2026-11-15T18:50:00Z".to_string()),
            last_modified_by: Some("Samira Ortiz".to_string()),
            ..Default::default()
        },
    ]
}

fn projects_sub_items() -> Vec<DriveItem> {
    vec![
        DriveItem {
            id: "proj-fld-charts".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: Some("od-fld-projects".to_string()),
            parent_path: "/Projects/Platform-v4".to_string(),
            name: "helm-charts".to_string(),
            size: 0,
            is_folder: true,
            child_count: Some(5),
            last_modified_at: Some("2026-11-19T12:00:00Z".to_string()),
            last_modified_by: Some("Jordan Lee".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "proj-file-deploy".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: Some("od-fld-projects".to_string()),
            parent_path: "/Projects/Platform-v4".to_string(),
            name: "deploy-pipeline.yaml".to_string(),
            size: 3_640,
            is_folder: false,
            mime_type: Some("application/x-yaml".to_string()),
            web_url: Some("https://acme.example/onedrive/Projects/Platform-v4/deploy-pipeline.yaml".to_string()),
            last_modified_at: Some("2026-11-19T16:22:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            shared_scope: Some("Organization (edit)".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "proj-file-readme".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: Some("od-fld-projects".to_string()),
            parent_path: "/Projects/Platform-v4".to_string(),
            name: "README.md".to_string(),
            size: 5_120,
            is_folder: false,
            mime_type: Some("text/markdown".to_string()),
            web_url: Some("https://acme.example/onedrive/Projects/Platform-v4/README.md".to_string()),
            last_modified_at: Some("2026-11-18T17:40:00Z".to_string()),
            last_modified_by: Some("Maya Patel".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "proj-file-bench".to_string(),
            drive_id: "drv-me".to_string(),
            parent_id: Some("od-fld-projects".to_string()),
            parent_path: "/Projects/Platform-v4".to_string(),
            name: "wal-benchmarks.json".to_string(),
            size: 14_920,
            is_folder: false,
            mime_type: Some("application/json".to_string()),
            web_url: Some("https://acme.example/onedrive/Projects/Platform-v4/wal-benchmarks.json".to_string()),
            last_modified_at: Some("2026-11-17T10:15:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            ..Default::default()
        },
    ]
}

fn sharepoint_eng_docs_items() -> Vec<DriveItem> {
    vec![
        DriveItem {
            id: "sp-fld-rfcs".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "RFCs".to_string(),
            size: 0,
            is_folder: true,
            child_count: Some(19),
            web_url: Some("https://acme.example/sites/engineering/Docs/RFCs".to_string()),
            last_modified_at: Some("2026-11-19T14:55:00Z".to_string()),
            last_modified_by: Some("Maya Patel".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "sp-fld-runbooks".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "Runbooks".to_string(),
            size: 0,
            is_folder: true,
            child_count: Some(11),
            web_url: Some("https://acme.example/sites/engineering/Docs/Runbooks".to_string()),
            last_modified_at: Some("2026-11-18T20:10:00Z".to_string()),
            last_modified_by: Some("Jordan Lee".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "sp-file-rfc".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "api-gateway-rfc.md".to_string(),
            size: 8_640,
            is_folder: false,
            mime_type: Some("text/markdown".to_string()),
            web_url: Some("https://acme.example/sites/engineering/Docs/api-gateway-rfc.md".to_string()),
            last_modified_at: Some("2026-11-19T16:35:00Z".to_string()),
            last_modified_by: Some("Maya Patel".to_string()),
            shared_scope: Some("Organization (edit)".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "sp-file-audit".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "security-audit-q4.pdf".to_string(),
            size: 1_840_200,
            is_folder: false,
            mime_type: Some("application/pdf".to_string()),
            web_url: Some("https://acme.example/sites/engineering/Docs/security-audit-q4.pdf".to_string()),
            last_modified_at: Some("2026-11-17T13:10:00Z".to_string()),
            last_modified_by: Some("Samira Ortiz".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "sp-file-slo".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "slo-dashboard.png".to_string(),
            size: 326_400,
            is_folder: false,
            mime_type: Some("image/png".to_string()),
            web_url: Some("https://acme.example/sites/engineering/Docs/slo-dashboard.png".to_string()),
            last_modified_at: Some("2026-11-16T08:40:00Z".to_string()),
            last_modified_by: Some("Jordan Lee".to_string()),
            ..Default::default()
        },
        DriveItem {
            id: "sp-file-tf".to_string(),
            drive_id: "drv-eng-docs".to_string(),
            parent_id: None,
            parent_path: String::new(),
            name: "terraform-modules.toml".to_string(),
            size: 4_110,
            is_folder: false,
            mime_type: Some("application/toml".to_string()),
            web_url: Some("https://acme.example/sites/engineering/Docs/terraform-modules.toml".to_string()),
            last_modified_at: Some("2026-11-14T16:05:00Z".to_string()),
            last_modified_by: Some("Alex Chen".to_string()),
            ..Default::default()
        },
    ]
}

fn build_demo_state() -> Result<(AppState, UiState)> {
    std::env::set_var("SHAREPOINT_CONFIG_DIR", r"~\.config\sharepoint-cli");

    let mut cfg = AppConfig::default();
    cfg.theme = ThemePreset::Dark;
    cfg.density = DensityMode::Cozy;
    cfg.show_nav_rail = true;
    cfg.inline_images = true;
    cfg.notify = true;
    cfg.download_dir = r"~\Downloads\sharepoint".to_string();

    let dummy_tokens = StoredTokens {
        access_token: "demo-token".to_string(),
        refresh_token: None,
        expires_at: 2_000_000_000,
        client_id: "demo-client".to_string(),
        tenant_id: "acme-corp".to_string(),
        user_id: Some("usr-alex-chen".to_string()),
        user_display_name: Some("Alex Chen".to_string()),
        user_email: Some("alex.chen@acme.example".to_string()),
        auth_mode: "demo".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), dummy_tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let storage = Storage::open_in_memory()?;
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let mut app = AppState::new(
        cfg.clone(),
        Persona::OneDrive,
        auth,
        graph,
        storage,
        sync_ctrl,
        ui_tx,
    );
    app.download_dir = PathBuf::from(r"~\Downloads\sharepoint");
    app.me = Some(UserProfile {
        id: "usr-alex-chen".to_string(),
        display_name: "Alex Chen".to_string(),
        email: Some("alex.chen@acme.example".to_string()),
        job_title: Some("Staff Systems Engineer".to_string()),
        department: Some("Platform Engineering".to_string()),
    });
    app.my_drive = Some(DriveInfo {
        id: "drv-me".to_string(),
        name: "OneDrive".to_string(),
        drive_type: "business".to_string(),
        owner_name: Some("Alex Chen".to_string()),
        quota_total: Some(1_099_511_627_776),
        quota_used: Some(84_210_000_000),
        ..Default::default()
    });
    app.sync_summary = "Synced · FTS5 ready".to_string();

    app.pins = vec![PinnedTarget {
        key: "pin-eng-docs".to_string(),
        kind: "folder".to_string(),
        drive_id: Some("drv-eng-docs".to_string()),
        item_id: None,
        site_id: Some("site-eng".to_string()),
        label: "Engineering Docs".to_string(),
    }];

    app.sites = vec![
        SharePointSite {
            id: "site-eng".to_string(),
            name: "engineering".to_string(),
            display_name: "Acme Engineering".to_string(),
            description: Some("Core platform docs, RFCs & release artifacts".to_string()),
            web_url: Some("https://acme.example/sites/engineering".to_string()),
            is_followed: true,
            ..Default::default()
        },
        SharePointSite {
            id: "site-prod".to_string(),
            name: "product".to_string(),
            display_name: "Product Hub".to_string(),
            description: Some("Quarterly roadmaps & launch plans".to_string()),
            web_url: Some("https://acme.example/sites/product".to_string()),
            is_followed: true,
            ..Default::default()
        },
    ];

    app.site_drives.insert(
        "site-eng".to_string(),
        vec![
            DriveInfo {
                id: "drv-eng-docs".to_string(),
                name: "Engineering Docs".to_string(),
                drive_type: "documentLibrary".to_string(),
                site_id: Some("site-eng".to_string()),
                site_name: Some("Acme Engineering".to_string()),
                ..Default::default()
            },
            DriveInfo {
                id: "drv-eng-rel".to_string(),
                name: "Release Artifacts".to_string(),
                drive_type: "documentLibrary".to_string(),
                site_id: Some("site-eng".to_string()),
                site_name: Some("Acme Engineering".to_string()),
                ..Default::default()
            },
        ],
    );
    app.site_drives.insert(
        "site-prod".to_string(),
        vec![DriveInfo {
            id: "drv-prod-specs".to_string(),
            name: "Roadmap Specs".to_string(),
            drive_type: "documentLibrary".to_string(),
            site_id: Some("site-prod".to_string()),
            site_name: Some("Product Hub".to_string()),
            ..Default::default()
        }],
    );

    // Preload realistic text previews & version histories
    app.text_previews.insert(
        "od-file-roadmap".to_string(),
        "# Q4 Platform Roadmap\n\n\
         ## Key Milestones\n\
         - [x] Ship SQLite WAL 4-reader pool\n\
         - [x] Enable Kitty / Sixel HD previews\n\
         - [ ] Roll out zero-downtime edge sync\n\n\
         ## Links\n\
         - [Architecture RFC](https://docs.acme.example/rfc-412)\n\
         - [SLO Dashboard](https://metrics.acme.example/platform)"
            .to_string(),
    );
    app.item_versions.insert(
        "od-file-roadmap".to_string(),
        vec![
            DriveItemVersion {
                id: "3.0".to_string(),
                last_modified_at: Some("2026-11-19T15:45:00Z".to_string()),
                last_modified_by: Some("Alex Chen".to_string()),
                size: 4_820,
            },
            DriveItemVersion {
                id: "2.0".to_string(),
                last_modified_at: Some("2026-11-14T11:20:00Z".to_string()),
                last_modified_by: Some("Maya Patel".to_string()),
                size: 4_310,
            },
        ],
    );

    app.text_previews.insert(
        "proj-file-deploy".to_string(),
        "name: platform-v4-canary\n\
         on:\n\
           push:\n\
             branches: [main]\n\
         stages:\n\
           - id: verify-wal\n\
             timeout: 5m\n\
             slo_p99_ms: 15\n\
           - id: canary-us-east\n\
             traffic_pct: 25\n\
             rollback_on_5xx: true"
            .to_string(),
    );
    app.item_versions.insert(
        "proj-file-deploy".to_string(),
        vec![
            DriveItemVersion {
                id: "4.2".to_string(),
                last_modified_at: Some("2026-11-19T16:22:00Z".to_string()),
                last_modified_by: Some("Alex Chen".to_string()),
                size: 3_640,
            },
            DriveItemVersion {
                id: "4.1".to_string(),
                last_modified_at: Some("2026-11-17T09:10:00Z".to_string()),
                last_modified_by: Some("Jordan Lee".to_string()),
                size: 3_480,
            },
        ],
    );

    app.text_previews.insert(
        "sp-file-rfc".to_string(),
        "# API Gateway v2 RFC\n\n\
         Owner: Platform Architecture\n\
         Spec: [OpenAPI Contract](https://api.acme.example/v2/openapi)\n\
         Telemetry: [Grafana SLOs](https://metrics.acme.example/gateway)\n\
         Runbook: https://runbooks.acme.example/gateway-failover\n\n\
         ## Summary\n\
         - Streaming chunked uploads (> 4 MiB)\n\
         - Per-tenant adaptive rate limiting\n\
         - Sub-10ms local FTS5 metadata lookup"
            .to_string(),
    );
    app.created_share_links.insert(
        "sp-file-rfc".to_string(),
        "https://acme.example/:w:/s/eng/api-gateway-rfc-v2".to_string(),
    );
    app.item_versions.insert(
        "sp-file-rfc".to_string(),
        vec![
            DriveItemVersion {
                id: "5.0".to_string(),
                last_modified_at: Some("2026-11-19T16:35:00Z".to_string()),
                last_modified_by: Some("Maya Patel".to_string()),
                size: 8_640,
            },
            DriveItemVersion {
                id: "4.0".to_string(),
                last_modified_at: Some("2026-11-16T10:00:00Z".to_string()),
                last_modified_by: Some("Alex Chen".to_string()),
                size: 7_920,
            },
        ],
    );

    app.active_source = ActiveViewSource::OneDriveRoot;
    app.current_items = onedrive_root_items();
    app.active_selected_id = Some("od-file-roadmap".to_string());

    let mut ui = UiState::from_config(&cfg);
    ui.focus = Focus::List;
    ui.theme_preset = ThemePreset::Dark;
    ui.theme = Theme::midnight_dark();
    ui.graphics_caps = TerminalGraphicsCaps {
        protocol: TerminalImageProtocol::Kitty,
        ..TerminalGraphicsCaps::default()
    };
    ui.list_scroll.selected_idx = Some(3); // q4-roadmap.md in sorted order (2 folders + 2nd file? let's compute exact index)

    Ok((app, ui))
}

fn select_item_by_id(app: &mut AppState, ui: &mut UiState, id: &str) {
    let items = app.sorted_visible_items();
    let idx = items.iter().position(|i| i.id == id).unwrap_or(0);
    app.active_selected_id = items.get(idx).map(|i| i.id.clone());
    ui.list_scroll.selected_idx = Some(idx);
    ui.inspector_scroll.offset = 0;
}

fn build_demo_file_picker() -> FilePickerOverlay {
    let base = PathBuf::from("~/workspace/acme-platform");
    FilePickerOverlay {
        current_dir: base.clone(),
        entries: vec![
            FilePickerEntry {
                name: "diagrams".to_string(),
                path: base.join("diagrams"),
                is_dir: true,
                size_bytes: 0,
            },
            FilePickerEntry {
                name: "specs".to_string(),
                path: base.join("specs"),
                is_dir: true,
                size_bytes: 0,
            },
            FilePickerEntry {
                name: "q4-architecture-deck.pdf".to_string(),
                path: base.join("q4-architecture-deck.pdf"),
                is_dir: false,
                size_bytes: 2_384_100,
            },
            FilePickerEntry {
                name: "gateway-benchmarks.csv".to_string(),
                path: base.join("gateway-benchmarks.csv"),
                is_dir: false,
                size_bytes: 24_510,
            },
            FilePickerEntry {
                name: "release-notes-v4.md".to_string(),
                path: base.join("release-notes-v4.md"),
                is_dir: false,
                size_bytes: 7_280,
            },
            FilePickerEntry {
                name: "slo-topology.png".to_string(),
                path: base.join("slo-topology.png"),
                is_dir: false,
                size_bytes: 318_920,
            },
        ],
        filter: String::new(),
        selected: 2,
        mode: FilePickerMode::Browse,
        zoxide_dirs: Vec::new(),
        zoxide_query: String::new(),
        zoxide_selected: 0,
        status: Some("Press z for zoxide jump · Enter to upload into Engineering Docs".to_string()),
        error: None,
    }
}

fn main() -> Result<()> {
    let (mut app, mut ui) = build_demo_state()?;
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend)?;

    let mut frames = Vec::new();

    // Frame 1: OneDrive 3-Pane Explorer (Sites/Drives | Files | Inspector Preview)
    select_item_by_id(&mut app, &mut ui, "od-file-roadmap");
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2200,
        "3-pane OneDrive & SharePoint TUI · Sites, Libraries, Files & live Inspector",
        "onedrive",
    )?);

    // Frame 2: Navigate into a folder / preview a YAML/code file (Enter / j / k)
    app.folder_stack = vec![
        ("od-fld-projects".to_string(), "Projects".to_string()),
        ("od-fld-v4".to_string(), "Platform-v4".to_string()),
    ];
    app.current_items = projects_sub_items();
    select_item_by_id(&mut app, &mut ui, "proj-file-deploy");
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Navigate nested folders with instant Markdown, YAML, JSON & code preview",
        "Enter / j / k",
    )?);

    // Frame 3: Switch to SharePoint Site library (Engineering Docs)
    app.persona = Persona::SharePoint;
    app.active_source = ActiveViewSource::Drive {
        drive_id: "drv-eng-docs".to_string(),
        label: "Acme Engineering / Engineering Docs".to_string(),
    };
    app.folder_stack.clear();
    app.current_items = sharepoint_eng_docs_items();
    select_item_by_id(&mut app, &mut ui, "sp-file-rfc");
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2000,
        "Switch seamlessly to SharePoint Site Document Libraries with version history",
        "sharepoint",
    )?);

    // Frame 4: Open Link Extractor overlay (u)
    if let Some(item) = app.active_item() {
        let share_url = app.created_share_links.get(&item.id).map(String::as_str);
        let preview = app.text_previews.get(&item.id).map(String::as_str);
        let mut links_ov = LinkExtractorOverlay::from_drive_item(&item, share_url, preview);
        links_ov.selected = 2;
        ui.overlays.push(Overlay::links(links_ov));
    }
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Link Extractor · inspect, copy (y) or open (Enter) share, web & document URLs",
        "u",
    )?);
    ui.overlays.clear();

    // Frame 5: Open Upload / File Picker overlay (Ctrl+F)
    ui.overlays.push(Overlay::FilePicker(build_demo_file_picker()));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Interactive Upload File Picker · type to filter or press z for zoxide jump",
        "Ctrl+F",
    )?);
    ui.overlays.clear();

    // Frame 6: Open Command Palette (Ctrl+P)
    let mut palette = CommandPalette::new();
    palette.selected = 2;
    ui.overlays.push(Overlay::CommandPalette(palette));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Unified Command Palette · fuzzy jump across libraries, OneDrive & actions",
        "Ctrl+P",
    )?);
    ui.overlays.clear();

    // Frame 7: Open Context Menu (.)
    let sel_idx = ui.list_scroll.selected_idx.unwrap_or(2);
    let mut menu = ContextMenu::build(ContextTarget::DriveItem(sel_idx), (44, 7), &app, &ui);
    menu.selected = 1;
    ui.overlays.push(Overlay::context_menu(menu));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Context Menu (. / right-click) · download, share link, rename, pin & upload",
        ".",
    )?);
    ui.overlays.clear();

    // Frame 8: Open Settings overlay (Ctrl+,)
    ui.overlays.push(Overlay::Settings(SettingsState { selected: 3 }));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2100,
        "Live Settings · themes, list density, Kitty / iTerm2 / Sixel / Halfblock images",
        "Ctrl+,",
    )?);
    ui.overlays.clear();

    let spec = json!({
        "title": "sharepoint / onedrive — Acme Engineering · Microsoft 365 Files TUI",
        "badge": "@shinick/sharepoint v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [17, 19, 24],
        "fg": [226, 232, 240],
        "frames": frames,
    });

    let frames_path = std::env::temp_dir().join("sharepoint_demo_frames.json");
    std::fs::write(&frames_path, serde_json::to_vec(&spec)?)
        .with_context(|| format!("Failed to write {}", frames_path.display()))?;

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out_gif = repo_root.join("assets").join("demo.gif");

    let status = Command::new("python")
        .arg(RENDER_SCRIPT)
        .arg(&frames_path)
        .arg(&out_gif)
        .arg("--preview-png")
        .arg(PREVIEW_DIR)
        .status()
        .context("Failed to execute render_tui_gif.py")?;

    if !status.success() {
        bail!("render_tui_gif.py exited with status {status}");
    }

    println!("Wrote frames spec: {}", frames_path.display());
    println!("Wrote demo GIF:    {}", out_gif.display());
    println!("Preview PNG dir:   {PREVIEW_DIR}");
    Ok(())
}
