use cli_command_kit::Mode;
use std::io;
use workspace_overlay::Cancellation;
use workspace_overlay_cli::{Result, finish, fixture, report, signals::SignalsGuard};

fn execute() -> Result<()> {
    let cancel = Cancellation::default();
    let mut signals = SignalsGuard::new(cancel.clone())?;
    finish(
        fixture::run(
            std::env::args_os().skip(1),
            Mode::from_env(),
            &cancel,
            &mut io::stdout().lock(),
        ),
        signals.close(),
    )
}
fn main() {
    if let Err(error) = execute() {
        let _ = report(error, &mut io::stderr().lock());
        std::process::exit(1);
    }
}
