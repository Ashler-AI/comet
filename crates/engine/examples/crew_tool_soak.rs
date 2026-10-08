use std::sync::Arc;
use std::time::Duration;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::{AgentEvent, DoneStatus, HarnessId, RunRequest, RuntimeProfile, SandboxLevel, SessionStatus, ToolCall};

fn rss_kib() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-p", &std::process::id().to_string(), "-o", "rss="])
        .output().unwrap();
    assert!(output.status.success());
    std::str::from_utf8(&output.stdout).unwrap().trim().parse().unwrap()
}

#[cfg(target_os = "macos")]
fn malloc_usage() -> (usize, usize) {
    // Matches malloc_statistics_t in the macOS SDK's malloc/malloc.h.
    #[repr(C)]
    struct Statistics {
        blocks_in_use: std::ffi::c_uint,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    unsafe extern "C" {
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut Statistics);
    }
    let mut stats = Statistics { blocks_in_use: 0, size_in_use: 0, max_size_in_use: 0, size_allocated: 0 };
    // A null zone sums all zones; this reads statistics, never trims memory.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats); }
    (stats.size_in_use, stats.size_allocated)
}

#[tokio::main]
async fn main() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = 0x1234_5678_u32;
    let command: String = (0..300 * 1024).map(|_| {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        char::from(32 + ((state >> 24) % 95) as u8)
    }).collect();
    let mut script: Vec<_> = (4096..=command.len()).step_by(4096).map(|end| AgentEvent::ToolCall {
        id: "large-tool".into(), call: ToolCall::Exec { command: command[..end].into() },
    }).collect();
    script.push(AgentEvent::ToolResult { id: "large-tool".into(), is_error: false, output: Some("completed".into()) });
    script.push(AgentEvent::Done { status: DoneStatus::Completed, result: None, error: None, session_id: None });
    let registry = HarnessRegistry::for_profile(RuntimeProfile::Mock);
    registry.register(Arc::new(MockHarness { script }));
    let core = EngineCore::assemble(dir.path(), Arc::new(registry), HarnessId::Mock, None).unwrap();
    let chat = "00000000-0000-4000-8000-000000000911";
    core.workspace.claim_chat(chat, Some("/tmp")).unwrap();
    let mut statuses = core.sessions.watch_sessions();
    let mut rss = Vec::new();
    for turn in 0..24 {
        let request = RunRequest { prompt: format!("large tool turn {turn}"), model: None,
            agent_account_id: None, reasoning: None, model_options: Default::default(), cwd: "/tmp".into(),
            sandbox: SandboxLevel::WorkspaceWrite, auto_approve: true, attachments: Vec::new(), resume: None };
        core.sessions.dispatch(chat, HarnessId::Mock, request, None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if statuses.borrow_and_update().iter().any(|session| session.chat_id == chat && session.status == SessionStatus::Idle) { break }
                statuses.changed().await.unwrap();
            }
        }).await.unwrap();
        let handle = core.doc_host.open(chat).unwrap();
        let entries = handle.doc().read_entries().unwrap();
        assert_eq!(entries.iter().filter(|entry| entry.role == comet_doc::MessageRole::Assistant
            && entry.status == Some(comet_doc::MessageStatus::Complete)).count(), turn + 1);
        let assistant = entries.iter().rev().find(|entry| entry.role == comet_doc::MessageRole::Assistant).unwrap();
        assert_eq!(assistant.status, Some(comet_doc::MessageStatus::Complete));
        assert!(assistant.parts.iter().any(|part| matches!(part, comet_doc::MessagePart::Tool {
            call: ToolCall::Exec { command: text }, resolved: true, ..
        } if text == &command)));
        drop(entries);
        let current_rss = rss_kib();
        eprintln!("turn={} rssKiB={current_rss}", turn + 1);
        #[cfg(target_os = "macos")]
        {
            let (in_use, reserved) = malloc_usage();
            eprintln!("turn={} mallocInUseBytes={in_use} mallocReservedBytes={reserved}", turn + 1);
        }
        if turn >= 3 { rss.push(current_rss); }
    }
    #[cfg(target_os = "macos")]
    {
        // Read-only native-region attribution, after all paced turns and RSS
        // samples. Do not export or analyze Loro history just to measure it.
        let summary = std::process::Command::new("vmmap")
            .args(["-summary", &std::process::id().to_string()])
            .output().expect("collect native memory-region summary");
        eprintln!("vmmap summary status={}:\n{}{}", summary.status,
            String::from_utf8_lossy(&summary.stdout), String::from_utf8_lossy(&summary.stderr));
    }
    let growth = rss.iter().max().unwrap() - rss[0];
    assert!(growth <= 128 * 1024, "actual engine retained {growth} KiB after warmup");
    println!("actual-engine large-tool soak: 24 completed turns, 300 KiB progressive arguments each, rssKiB={rss:?}, growthKiB={growth}");
    core.shutdown().await;
}
