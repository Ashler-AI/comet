//! Crew's desktop background engine, as a process of its own ("Crew Engine"
//! in Activity Monitor): the app starts it and attaches;
//! sessions keep running with the app closed. Without the UI linked in.
//!
//! It also answers what the engine runs its own executable for: `mcp` (the
//! Crew MCP server injected into agents), `--noop-browser` (browser
//! suppression for agents' sign-in flows) and the OMP writer probe and
//! supervisor (Linux resume and persistent runs).

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    if let Some(result) = zeron_harness::omp::run_writer_probe_from_env() {
        return result.map_err(anyhow::Error::from);
    }
    if let Some(result) = zeron_harness::omp::run_supervisor_from_env() {
        return result.map_err(anyhow::Error::from);
    }
    match std::env::args().nth(1).as_deref() {
        Some("--noop-browser") => Ok(()),
        Some("mcp") => {
            zeron_app::init_logging(None, "mcp", true);
            zeron_app::run_mcp()
        }
        Some("--version" | "-V") => {
            println!("comet-engine {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => zeron_app::background::run(),
    }
}
