use cli_command_kit::Mode;
use std::io::{self, Write};
use workspace_overlay::Cancellation;
use workspace_overlay_cli::{Result, finish, prompt, report, run, signals::SignalsGuard};

fn execute() -> Result<()> {
    env_logger::Builder::new()
        .filter_level(log::LevelFilter::Warn)
        .format(|output, record| {
            writeln!(
                output,
                "{}: {}",
                record.level(),
                cli_command_kit::display_message(&record.args().to_string())
            )
        })
        .try_init()
        .map_err(io::Error::other)?;
    let mode = Mode::from_env();
    let cancel = Cancellation::default();
    let mut signals = SignalsGuard::new(cancel.clone())?;
    let confirm = prompt::confirmation(mode);
    let result = run(
        std::env::args_os().skip(1),
        mode,
        &cancel,
        None,
        confirm.as_deref(),
        &mut io::stdout().lock(),
        &mut io::stderr(),
    );
    finish(result, signals.close())
}
fn main() {
    if let Err(error) = execute() {
        let _ = report(error, &mut io::stderr().lock());
        std::process::exit(1);
    }
}
