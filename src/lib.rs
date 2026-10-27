//! Command parsing and terminal boundaries for native workspace overlays.
pub mod fixture;
pub mod prompt;
pub mod signals;
use cli_command_kit::{
    HelpOptions, Mode,
    clap::{
        Arg, ArgAction, Command,
        builder::{OsStringValueParser, TypedValueParser},
        error::{ContextKind, ContextValue, ErrorKind},
    },
};
use std::{
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
};
use workspace_overlay::{
    Cancellation,
    config::Configuration,
    management,
    mounts::Confirm,
    registry::Store,
    session::{self, Event},
};

pub const SKILL: &str = include_str!("../skills/workspace-overlay/SKILL.md");
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Arguments(#[from] cli_command_kit::clap::Error),
    #[error(transparent)]
    Help(#[from] cli_command_kit::Error),
    #[error(transparent)]
    Overlay(#[from] workspace_overlay::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("{operation}; signal cleanup: {cleanup}")]
    Cleanup {
        #[source]
        operation: Box<Error>,
        cleanup: io::Error,
    },
}
pub type Result<T> = std::result::Result<T, Error>;

/// Retain an operation failure and an independent signal-thread cleanup failure.
pub fn finish(operation: Result<()>, cleanup: io::Result<()>) -> Result<()> {
    match (operation, cleanup) {
        (Ok(()), result) => result.map_err(Error::from),
        (result, Ok(())) => result,
        (Err(operation), Err(cleanup)) => Err(Error::Cleanup {
            operation: Box::new(operation),
            cleanup,
        }),
    }
}

/// Report usage for a parse failure, then print the returned error once.
pub fn report(mut error: Error, output: &mut impl Write) -> io::Result<()> {
    let primary = match &mut error {
        Error::Cleanup { operation, .. } => &mut **operation,
        other => other,
    };
    if let Error::Arguments(arguments) = primary
        && let Some(usage) = arguments.remove(ContextKind::Usage)
    {
        writeln!(
            output,
            "{}",
            cli_command_kit::display_message(&usage.to_string())
        )?;
    }
    writeln!(
        output,
        "ERR: {}",
        cli_command_kit::display_message(&error.to_string())
    )
}

pub fn help_options() -> HelpOptions {
    HelpOptions {
        skill: Some(SKILL.into()),
        ..HelpOptions::default()
    }
}

pub fn command(mode: Mode) -> Result<Command> {
    let mut overlay = Command::new("overlay")
        .about("Manage configured project overlays")
        .arg(Arg::new("config").long("config").value_name("PATH").value_parser(OsStringValueParser::new().map(PathBuf::from)).default_value("").global(true).help("TOML file; empty discovers workspace-overlay.toml in current or parent directories"))
        .arg(Arg::new("project").long("project").value_name("NAME").default_value("").global(true).help("Configured project name; empty selects all projects"));
    for action in ["mount", "unmount", "status"] {
        let mut child = Command::new(action)
            .about(format!("{action} configured project overlays"))
            .arg(
                Arg::new("worktrees")
                    .long("worktrees")
                    .action(ArgAction::Set)
                    .value_parser(cli_command_kit::clap::value_parser!(bool))
                    .num_args(0..=1)
                    .require_equals(true)
                    .default_missing_value("true")
                    .default_value("true")
                    .help("Include and monitor Git worktrees of each project"),
            );
        if action == "mount" {
            child = child.arg(
                Arg::new("replace")
                    .long("replace")
                    .action(ArgAction::SetTrue)
                    .help("Stop existing overlays before starting"),
            );
        }
        overlay = overlay.subcommand(child);
    }
    let root = Command::new("workspace-overlay")
        .about("Serve agent files from workspace sources")
        .disable_version_flag(true)
        .arg(
            Arg::new("version")
                .long("version")
                .short('v')
                .action(ArgAction::SetTrue)
                .help("Print the raw version"),
        )
        .subcommand(overlay);
    Ok(cli_command_kit::setup(root, mode, &help_options())?)
}

/// Run an invocation with explicit cancellation, state and terminal streams.
/// Parsing failures retain Clap's usage diagnostic; operation failures do not.
pub fn run(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    mode: Mode,
    cancel: &Cancellation,
    store: Option<&Store>,
    confirm: Option<&Confirm<'_>>,
    output: &mut impl Write,
    diagnostic: &mut impl Write,
) -> Result<()> {
    let root = command(mode)?;
    let Some(matches) = parse(&root, args, output)? else {
        return Ok(());
    };
    if matches.get_flag("version") {
        writeln!(output, "{VERSION}")?;
        return Ok(());
    }
    if cli_command_kit::write_skill(&matches, &help_options(), output)? {
        return Ok(());
    }
    let Some(("overlay", group)) = matches.subcommand() else {
        output.write_all(root.clone().render_help().to_string().as_bytes())?;
        return Ok(());
    };
    let Some((action, flags)) = group.subcommand() else {
        output.write_all(
            root.find_subcommand("overlay")
                .expect("defined overlay command")
                .clone()
                .render_help()
                .to_string()
                .as_bytes(),
        )?;
        return Ok(());
    };
    let config = Configuration::discover(group.get_one::<PathBuf>("config").map(PathBuf::as_path))?;
    let selections = config.selections(group.get_one::<String>("project").map(String::as_str))?;
    let environment;
    let store = match store {
        Some(store) => store,
        None => {
            environment = Store::from_environment()?;
            &environment
        }
    };
    let worktrees = flags.get_flag("worktrees");
    if action == "mount" {
        session::serve(
            selections,
            session::Options {
                replace: flags.get_flag("replace"),
                worktrees,
            },
            store.clone(),
            cancel,
            confirm,
            &mut |event| emit(event, diagnostic),
        )?;
    } else {
        let mut errors = Vec::new();
        for selection in selections {
            let result = if action == "status" {
                management::status(store, &selection, worktrees, cancel).and_then(|rows| {
                    for row in rows {
                        writeln!(
                            output,
                            "{}\t{}",
                            cli_command_kit::display_path(&row.target),
                            row.filesystem
                                .as_deref()
                                .map_or_else(|| "unmounted".into(), |name| name.to_string_lossy())
                        )?;
                    }
                    Ok(())
                })
            } else {
                management::unmount(store, &selection, worktrees, confirm, cancel)
            };
            if let Err(error) = result {
                errors.push(error);
            }
        }
        match errors.len() {
            0 => {}
            1 => return Err(errors.remove(0).into()),
            _ => {
                return Err(
                    workspace_overlay::Error::Failures(workspace_overlay::Failures(errors)).into(),
                );
            }
        }
    }
    Ok(())
}

fn parse(
    root: &Command,
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    output: &mut impl Write,
) -> Result<Option<cli_command_kit::clap::ArgMatches>> {
    match root.clone().no_binary_name(true).try_get_matches_from(args) {
        Ok(matches) => Ok(Some(matches)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            output.write_all(error.to_string().as_bytes())?;
            Ok(None)
        }
        Err(mut error) => {
            if error.get(ContextKind::Usage).is_none() {
                error.insert(
                    ContextKind::Usage,
                    ContextValue::StyledStr(root.clone().render_usage()),
                );
            }
            Err(error.into())
        }
    }
}

fn emit(event: Event, output: &mut impl Write) -> workspace_overlay::Result<()> {
    match event {
        Event::Mounted { target, sources } => {
            let paths = std::iter::once(target)
                .chain(sources)
                .map(|path| cli_command_kit::display_path(&path))
                .collect::<Vec<_>>();
            writeln!(output, "Mounted {}; Ctrl-C to unmount.", paths.join(" + "))?;
        }
        Event::Unmounted { target } => writeln!(
            output,
            "Unmounted {}",
            cli_command_kit::display_path(&target)
        )?,
        Event::Skipped { target, reason } => writeln!(
            output,
            "Skipping worktree {}: {}",
            cli_command_kit::display_path(&target),
            cli_command_kit::display_message(&reason)
        )?,
        Event::Warning(error) => writeln!(
            output,
            "WARN: {}",
            cli_command_kit::display_message(&error.to_string())
        )?,
    }
    output.flush()?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/events.rs"]
mod event_tests;
