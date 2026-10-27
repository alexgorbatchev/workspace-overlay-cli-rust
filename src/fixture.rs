//! Developer fixture command; the filesystem and Git implementation is shared.
use crate::{Result, VERSION, help_options, parse};
use cli_command_kit::{
    Mode,
    clap::{
        Arg, ArgAction, Command,
        builder::{OsStringValueParser, TypedValueParser},
    },
};
use std::{ffi::OsString, io::Write, os::unix::ffi::OsStrExt, path::PathBuf};
use workspace_overlay::Cancellation;

pub fn run(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    mode: Mode,
    cancel: &Cancellation,
    output: &mut impl Write,
) -> Result<()> {
    let root = Command::new("workspace-overlay-fixture")
        .about("Create or reuse an isolated overlay verification workspace")
        .arg(
            Arg::new("directory")
                .value_name("DIRECTORY")
                .default_value("dev-workspace")
                .value_parser(OsStringValueParser::new().map(PathBuf::from))
                .help("Directory to create or reuse"),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .short('v')
                .action(ArgAction::SetTrue)
                .help("Print the raw version"),
        );
    let root = cli_command_kit::setup(root, mode, &help_options())?;
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
    let directory = matches
        .get_one::<PathBuf>("directory")
        .expect("directory has a default");
    let config = workspace_overlay::fixture::create(directory, cancel)?;
    // Machine output is an exact native pathname, used as an argument by just.
    // Display-only paths elsewhere are folded through the shared CLI formatter.
    output.write_all(config.as_os_str().as_bytes())?;
    writeln!(output)?;
    Ok(())
}
