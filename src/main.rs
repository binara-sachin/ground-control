//! `groundcontrol` opens the TUI; `groundcontrol svc-exec` and `run-exec` are
//! the commands of a service's and a run's tmux window.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => groundcontrol::tui::run(),
        ["svc-exec", project, service] => {
            let code = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt.block_on(groundcontrol::services::worker::run(project, service)),
                Err(e) => {
                    eprintln!("groundcontrol: {e}");
                    1
                }
            };
            return std::process::ExitCode::from(code as u8);
        }
        ["run-exec"] => {
            let code = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt.block_on(groundcontrol::runs::worker::run()),
                Err(e) => {
                    eprintln!("groundcontrol: {e}");
                    1
                }
            };
            return std::process::ExitCode::from(code as u8);
        }
        ["--version" | "-V"] => {
            println!("groundcontrol {}", groundcontrol::model::VERSION);
            Ok(())
        }
        _ => {
            eprintln!(
                "usage: groundcontrol                               open the TUI\n       \
                 groundcontrol svc-exec <project> <service>   run a service (inside its tmux window)\n       \
                 groundcontrol run-exec                       run a pipeline run (inside its tmux window)"
            );
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
