//! `config.toml` must hold what the user set, not what one run happened to see in its
//! environment (SP-10). Runs the real binary against a private config directory.

use std::process::Command;

fn sharepoint_with_clean_env() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharepoint"));
    for (key, _) in std::env::vars_os() {
        let upper = key.to_string_lossy().to_ascii_uppercase();
        if ["SHAREPOINT_", "ONEDRIVE_", "TEAMS_", "AZURE_"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            cmd.env_remove(key);
        }
    }
    cmd
}

#[test]
fn login_token_file_persists_only_token_file() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        "graph_base_url = \"https://graph.microsoft.com/v1.0\"\nnotify = false\nfuture_setting = \"keep me\"\n",
    )
    .unwrap();
    let token_path = tmp.path().join("token.txt");
    std::fs::write(&token_path, "opaque-external-token").unwrap();

    let out = sharepoint_with_clean_env()
        // Settings for this one run. None of them may reach the file.
        .env("SHAREPOINT_GRAPH_BASE_URL", "http://127.0.0.1:9/mock/v1.0")
        .env("SHAREPOINT_CLIENT_ID", "client-id-from-env")
        .env("SHAREPOINT_IMAGE_PROTOCOL", "sixel")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", &cfg_dir)
        .env("SHAREPOINT_DATA_DIR", tmp.path().join("data"))
        .args(["login", "--token-file"])
        .arg(&token_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let table: toml::Table = toml::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(
        table["token_file"].as_str(),
        Some(token_path.to_string_lossy().as_ref())
    );
    assert_eq!(
        table["graph_base_url"].as_str(),
        Some("https://graph.microsoft.com/v1.0"),
        "the mock URL of this run must not replace the configured one"
    );
    assert_eq!(table["notify"].as_bool(), Some(false));
    assert_eq!(table["future_setting"].as_str(), Some("keep me"));
    assert_eq!(table.len(), 4, "nothing else may be written: {table:?}");
}

#[test]
fn login_token_file_leaves_an_unparsable_config_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg_dir = tmp.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let cfg_path = cfg_dir.join("config.toml");
    let broken = "notify = = true\n";
    std::fs::write(&cfg_path, broken).unwrap();
    let token_path = tmp.path().join("token.txt");
    std::fs::write(&token_path, "opaque-external-token").unwrap();

    let out = sharepoint_with_clean_env()
        .env("SHAREPOINT_DISABLE_KEYRING", "1")
        .env("SHAREPOINT_SHARE_M365_AUTH", "0")
        .env("SHAREPOINT_AUTO_DAEMON", "0")
        .env("SHAREPOINT_CONFIG_DIR", &cfg_dir)
        .env("SHAREPOINT_DATA_DIR", tmp.path().join("data"))
        .args(["login", "--token-file"])
        .arg(&token_path)
        .output()
        .unwrap();
    assert!(!out.status.success(), "a config that cannot be read back must not be replaced");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("does not parse"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&cfg_path).unwrap(), broken);
}
