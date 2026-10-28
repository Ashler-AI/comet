//! Synthetic ACP peer. Exercises real native stdio/spawn without an agent account.
use serde_json::{Value, json};
use std::io::{BufRead, Write};

fn emit(message: Value) {
    let mut out = std::io::stdout().lock();
    serde_json::to_writer(&mut out, &message).unwrap();
    writeln!(out).unwrap();
    out.flush().unwrap();
}

fn update(session: &str, text: String) {
    emit(json!({"jsonrpc":"2.0","method":"session/update","params":{
        "sessionId":session,"update":{"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":text}}
    }}));
}

fn main() {
    if let Some(result) = zeron_harness::omp::run_writer_probe_from_env() {
        result.unwrap();
        return;
    }
    // A broken test must not leave even a direct fixture process behind.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(20));
        std::process::exit(99);
    });
    let args: Vec<_> = std::env::args().collect();
    #[cfg(target_os = "linux")]
    if args.get(1).is_some_and(|arg| arg == "--ownership-stop") {
        use zeron_harness::{Harness, OmpHarness};
        let harness = OmpHarness::new().with_executable(&args[2]).with_session_dir(&args[3]);
        if let Err(error) = tokio::runtime::Runtime::new().unwrap().block_on(harness.stop_session(&args[4])) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(target_os = "linux")]
    if args.get(1).is_some_and(|arg| arg == "--ownership-states") {
        use zeron_harness::omp::{session_writer_state, session_writer_states, SessionWriterState};
        let paths: Vec<std::path::PathBuf> = std::env::args_os().skip(2).map(Into::into).collect();
        let encode = |states: Vec<SessionWriterState>| states.into_iter().map(|state| match state {
            SessionWriterState::Active => 'A',
            SessionWriterState::Inactive => 'I',
            SessionWriterState::Unknown => 'U',
        }).collect::<String>();
        println!("{}", encode(paths.iter().map(|path| session_writer_state(path)).collect()));
        println!("{}", encode(session_writer_states(&paths)));
        return;
    }
    #[cfg(target_os = "linux")]
    if args.get(1).is_some_and(|arg| arg == "--ownership-holder") {
        use std::os::fd::AsRawFd;
        let mode = &args[2];
        let file = std::fs::OpenOptions::new().read(true).write(mode == "writer").open(&args[3]).unwrap();
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0), 0);
            if mode == "lease" {
                assert_ne!(libc::signal(libc::SIGIO, libc::SIG_IGN), libc::SIG_ERR);
                assert_eq!(libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, libc::F_WRLCK), 0);
            }
        }
        println!("ready");
        std::io::stdout().flush().unwrap();
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
        return;
    }
    if args.get(1).is_some_and(|arg| arg == "--launch-report") {
        println!(
            "{}",
            json!({
                "argv": args.iter().skip(2).collect::<Vec<_>>(),
                "cwd": std::env::current_dir().unwrap(),
                "marker": std::env::var("ZERON_LAUNCH_MARKER").ok(),
                "removed": std::env::var("ZERON_LAUNCH_REMOVED").ok(),
                "unicode": std::env::var("ZERON_Ä_KEY").ok(),
                "path": std::env::var("PATH").ok(),
            })
        );
        return;
    }
    if args.get(1).is_some_and(|arg| arg == "--capture-output") {
        print!("captured stdout");
        eprint!("captured stderr");
        return;
    }
    #[cfg(windows)]
    if args.get(1).is_some_and(|arg| arg == "--job-owner") {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let _guard = runtime.enter();
        let path = std::env::current_dir()
            .unwrap()
            .join("descendant-pids.json");
        let mut command = zeron_harness::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--tree-child")
            .arg(&path)
            .stdin(zeron_harness::process::Stdio::null())
            .stdout(zeron_harness::process::Stdio::null())
            .stderr(zeron_harness::process::Stdio::null());
        let _owned = command.spawn().unwrap();
        let pids = await_tree(&path);
        println!("{}", serde_json::to_string(&pids).unwrap());
        std::io::stdout().flush().unwrap();
        let mut release = String::new();
        std::io::stdin().read_line(&mut release).unwrap();
        // Bypass Rust destructors: emulate an application exiting abruptly.
        std::process::exit(0);
    }
    if args.get(1).is_some_and(|arg| arg == "--tree-leaf") {
        std::thread::sleep(std::time::Duration::from_secs(20));
        return;
    }
    if args.get(1).is_some_and(|arg| arg == "--tree-child") {
        let grandchild = tree_command().arg("--tree-leaf").spawn().unwrap();
        std::fs::write(
            &args[2],
            serde_json::to_vec(&vec![std::process::id(), grandchild.id()]).unwrap(),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_secs(20));
        return;
    }
    if args.get(1).is_some_and(|arg| arg == "--output-tree") {
        let _tree = spawn_tree();
        print!("{}FINAL-STDOUT", "o".repeat(128 * 1024));
        eprint!("{}FINAL-STDERR", "e".repeat(128 * 1024));
        std::io::stdout().flush().unwrap();
        std::io::stderr().flush().unwrap();
        return;
    }
    let mut session = "native-session".to_string();
    let mut pending = None;
    let mut ignore_cancel = false;
    for line in std::io::stdin().lock().lines() {
        let message: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        match message["method"].as_str().unwrap_or_default() {
            "initialize" => emit(json!({"jsonrpc":"2.0","id":id,"result":{
                "protocolVersion":1,"agentCapabilities":{"loadSession":true}
            }})),
            "session/new" => emit(json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":session}})),
            "session/load" => {
                session = message["params"]["sessionId"].as_str().unwrap().to_string();
                emit(json!({"jsonrpc":"2.0","id":id,"result":{}}));
            }
            "session/prompt" => {
                let prompt = message["params"]["prompt"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                let descendants = if prompt.ends_with("-tree") {
                    spawn_tree()
                } else {
                    Vec::new()
                };
                update(
                    &session,
                    json!({
                        "pid":std::process::id(), "prompt":prompt, "descendants":descendants,
                        "cwd":std::env::current_dir().unwrap(),
                        "argv":std::env::args().skip(1).collect::<Vec<_>>()
                    })
                    .to_string(),
                );
                if prompt == "wait-for-cancel" || prompt.ends_with("-tree") {
                    ignore_cancel = prompt == "ignore-cancel-tree";
                    pending = Some(id);
                } else {
                    emit(json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}));
                    break;
                }
            }
            "session/cancel" => {
                if ignore_cancel {
                    continue;
                }
                emit(
                    json!({"jsonrpc":"2.0","id":pending.take().expect("active prompt"),
                    "result":{"stopReason":"cancelled"}}),
                );
                break;
            }
            "session/set_model" | "session/set_config_option" => {
                emit(json!({"jsonrpc":"2.0","id":id,"result":{}}))
            }
            other => panic!("unexpected method: {other}"),
        }
    }
}

fn tree_command() -> std::process::Command {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    command
}

fn spawn_tree() -> Vec<u32> {
    let path = std::env::current_dir()
        .unwrap()
        .join("descendant-pids.json");
    let _child = tree_command()
        .arg("--tree-child")
        .arg(&path)
        .spawn()
        .unwrap();
    await_tree(&path)
}

fn await_tree(path: &std::path::Path) -> Vec<u32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(pids) = serde_json::from_slice(&bytes) {
                return pids;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "descendant startup timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
