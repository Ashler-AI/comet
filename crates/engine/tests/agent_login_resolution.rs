#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use comet_engine::scaffold::ScaffoldClient;
use comet_engine::{AgentAccounts, AgentAccountsConfig};
use comet_proto::{AgentLoginMode, HarnessId};

struct StaticToken;

#[async_trait]
impl comet_rpc::TokenSource for StaticToken {
    async fn token(&self) -> Option<String> {
        Some("test-access-token".into())
    }
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[tokio::test]
async fn provider_logins_work_from_a_gui_launch_path() {
    let dir = tempfile::tempdir().unwrap();
    let shell_bin = dir.path().join("shell-bin");
    std::fs::create_dir(&shell_bin).unwrap();
    write_executable(
        &shell_bin.join("codex"),
        "#!/bin/sh\nprintf '%s\\n' 'Open https://auth.openai.com/authorize?test=1' >&2\nexec /bin/sleep 30\n",
    );
    let fake_shell = dir.path().join("fake-shell");
    write_executable(
        &fake_shell,
        &format!(
            "#!/bin/sh\nPATH=\"{}:/usr/bin:/bin\"; export PATH\n\
             while [ \"$#\" -gt 0 ]; do\n\
               if [ \"$1\" = \"-c\" ]; then shift; exec /bin/sh -c \"$1\"; fi\n\
               shift\n\
             done\nexit 1\n",
            shell_bin.display()
        ),
    );

    // SAFETY: this integration-test binary contains one test, so no sibling can
    // observe the temporary GUI-style environment or initialize the shell cache.
    unsafe {
        std::env::set_var("SHELL", &fake_shell);
        std::env::set_var("HOME", dir.path());
        std::env::set_var("PATH", "/usr/bin:/bin");
        std::env::remove_var("CODEX_EXECUTABLE");
        std::env::remove_var("COMET_NO_LOGIN_SHELL");
    }

    let accounts = AgentAccounts::new(AgentAccountsConfig {
        data_dir: dir.path().join("data"),
        claude_config_dir: dir.path().join("claude"),
        claude_config_file: dir.path().join("claude.json"),
        codex_home: dir.path().join("codex"),
    });
    accounts.set_remote(
        ScaffoldClient::new("http://127.0.0.1:1", "project-1", Arc::new(StaticToken)).unwrap(),
    );

    let anthropic = accounts.start_login(HarnessId::ClaudeCode).await.unwrap();
    assert_eq!(anthropic.mode, AgentLoginMode::PasteCode);
    assert!(
        anthropic
            .url
            .starts_with("https://claude.ai/oauth/authorize?")
    );
    accounts.cancel_login(&anthropic.login_id);

    let openai = accounts.start_login(HarnessId::Codex).await.unwrap();
    assert_eq!(openai.mode, AgentLoginMode::Browser);
    assert_eq!(openai.url, "https://auth.openai.com/authorize?test=1");
    accounts.cancel_login(&openai.login_id);
}
