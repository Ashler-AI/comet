//! `comet login`, `logout`, and `status` for the persisted Scaffold OAuth session.

use std::io::IsTerminal;

use comet_engine::{AuthState, Engine, EngineConfig, InstanceLock};

pub async fn login(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    if !auth.oauth_enabled() {
        println!("Crew is using explicit local development identity.");
        return Ok(());
    }
    if let AuthState::SignedIn {
        user,
        project_scope,
    } = auth.state()
    {
        println!("Already signed in as {} ({project_scope}).", user.email);
        println!("Run `comet logout` first to switch accounts.");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("comet login needs an interactive terminal");
    }
    let _lock = engine_lock(&config, "sign in")?;
    comet_engine::terminal_sign_in(&auth).await?;
    match auth.state() {
        AuthState::SignedIn {
            user,
            project_scope,
        } => {
            println!("\nSigned in as {} ({project_scope}).", user.email);
            println!("Session saved. `comet headless` and the daemon will use it.");
        }
        AuthState::SignedOut => println!("Sign-in did not complete."),
    }
    Ok(())
}

pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    let _lock = engine_lock(&config, "sign out")?;
    if !auth.oauth_enabled() {
        auth.sign_out();
        println!("Cleared any saved session. Local development identity remains active.");
        return Ok(());
    }
    match auth.state() {
        AuthState::SignedOut => println!("No saved session."),
        AuthState::SignedIn { user, .. } => {
            auth.sign_out();
            println!(
                "Signed out {} and removed {}.",
                user.email,
                config.data_dir.join("session.json").display()
            );
        }
    }
    Ok(())
}

pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config).await;
    println!("Data dir: {}", config.data_dir.display());
    println!("Edge:     {}", config.edge_url);
    println!("Project:  {}", config.project_scope);
    let signed_in = match (auth.oauth_enabled(), auth.state()) {
        (false, _) => {
            println!("Auth:     local development identity");
            true
        }
        (
            true,
            AuthState::SignedIn {
                user,
                project_scope,
            },
        ) => {
            println!("Auth:     signed in as {} ({project_scope})", user.email);
            true
        }
        (true, AuthState::SignedOut) => {
            println!("Auth:     signed out; run `comet login`");
            false
        }
    };
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    if !signed_in {
        std::process::exit(1);
    }
    Ok(())
}

fn engine_lock(config: &EngineConfig, verb: &str) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(&config.data_dir).map_err(|err| {
        anyhow::anyhow!(
            "{err}\nCannot {verb} while an engine is running; stop it first (`comet daemon stop`, or quit Crew)."
        )
    })
}
