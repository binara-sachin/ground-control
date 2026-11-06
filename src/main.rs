//! `groundcontrol` opens the TUI; `groundcontrol daemon` runs the daemon;
//! `groundcontrol svc-exec` is the command of a service's tmux window.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => groundcontrol::tui::run(),
        ["daemon"] => tokio::runtime::Runtime::new().map_err(Into::into).and_then(|rt| rt.block_on(groundcontrol::daemon::run())),
        ["svc-exec", project, service, launch] => {
            let code = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt.block_on(groundcontrol::services::worker::run(project, service, launch)),
                Err(e) => {
                    eprintln!("groundcontrol: {e}");
                    1
                }
            };
            return std::process::ExitCode::from(code as u8);
        }
        ["--version" | "-V"] => {
            println!("groundcontrol {} (protocol {})", groundcontrol::protocol::VERSION, groundcontrol::protocol::PROTOCOL_VERSION);
            Ok(())
        }
        _ => {
            eprintln!("usage: groundcontrol            open the TUI\n       groundcontrol daemon     run the daemon (started by launchd)");
            return std::process::ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("groundcontrol: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
