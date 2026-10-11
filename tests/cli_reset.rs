//! Exercise the real binary under a controlling pseudo-terminal, including a live server.
#[cfg(unix)]
#[tokio::test]
async fn terminal_reset_and_live_server_races() {
    use stalias::{password::PasswordProcessor, store::Store};
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("stalias-cli-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Store::open(&dir.join("stalias.db")).unwrap();
    let hash = PasswordProcessor::default()
        .reserve()
        .unwrap()
        .hash("pflaume-Nebel-7-Kupfer-Zitrone".into())
        .await
        .unwrap();
    store.create_admin(hash.clone()).await.unwrap();
    store
        .set_stalwart("http://127.0.0.1:1".into(), "saved-key".into())
        .await
        .unwrap();
    store.set_domains(&["example.org".into()]).await.unwrap();
    store
        .record_alias_created("acc1", "shop@example.org")
        .await
        .unwrap();
    let token = store
        .create_token("manager".into(), "acc1".into(), "alice".into())
        .await
        .unwrap();
    let session = store.create_session(hash).await.unwrap().unwrap();
    let data_dir = dir.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/cli_reset_pty.py"
            ))
            .arg(env!("CARGO_BIN_EXE_stalias"))
            .arg(data_dir)
            .arg(session)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    // Keep fixture credentials and terminal output out of failure logs.
    let success = output.status.success();
    if !success {
        eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(store.find_token(&token).await.unwrap().is_some());
    let settings = store.settings().await.unwrap();
    assert_eq!(settings.stalwart_key.as_deref(), Some("saved-key"));
    assert_eq!(settings.domains, ["example.org"]);
    assert_eq!(store.alias_created_dates("acc1").await.unwrap().len(), 1);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
    assert!(success, "PTY reset regression failed");
}
