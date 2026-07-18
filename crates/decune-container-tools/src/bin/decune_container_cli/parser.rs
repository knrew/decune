use std::ffi::OsString;

pub const ROOT_HELP: &str = "\
Run decune queries for the current workspace from inside a container.

Usage: decune <COMMAND>

Supported commands:
  status  Show status for the current workspace
  ports   List active ports for the current workspace

Host-only commands:
  up, rebuild, down, remove, rm, clean
          Run these commands on the host

Options:
  -h, --help     Print help
  -V, --version  Print version
";

const STATUS_HELP: &str = "\
Show status for the current workspace.

Usage: decune status

This command always queries the current workspace inside the container.

Options:
  -h, --help  Print help
";

const PORTS_HELP: &str = "\
List active ports for the current workspace.

Usage: decune ports [--json]

This command always queries the current workspace inside the container.

Options:
      --json  Output active ports as JSON
  -h, --help  Print help
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryCommand {
    Status,
    Ports,
}

impl QueryCommand {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Ports => "ports",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryFormat {
    Text,
    Json,
}

impl QueryFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Query {
    pub command: QueryCommand,
    pub format: QueryFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpCommand {
    Root,
    Status,
    Ports,
    HostOnly(&'static str),
}

impl HelpCommand {
    pub fn text(self) -> String {
        match self {
            Self::Root => ROOT_HELP.to_owned(),
            Self::Status => STATUS_HELP.to_owned(),
            Self::Ports => PORTS_HELP.to_owned(),
            Self::HostOnly(command) => format!(
                "\
`decune {command}` can only be run on the host.

Run `decune {command} --help` on the host for usage details.
"
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedCommand {
    Query(Query),
    PrintHelp(HelpCommand),
    PrintVersion,
}

#[derive(Debug, PartialEq, Eq)]
pub struct UsageError {
    pub message: String,
    pub show_root_help: bool,
}

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            show_root_help: false,
        }
    }

    fn missing_command() -> Self {
        Self {
            message: "decune command is required inside a container".to_owned(),
            show_root_help: true,
        }
    }
}

pub fn parse(args: &[OsString]) -> Result<ParsedCommand, UsageError> {
    let Some(args) = args
        .iter()
        .map(|argument| argument.to_str())
        .collect::<Option<Vec<_>>>()
    else {
        return Err(UsageError::new(
            "decune arguments must contain valid UTF-8 inside a container",
        ));
    };
    let Some(command) = args.first().copied() else {
        return Err(UsageError::missing_command());
    };

    match command {
        "--help" | "-h" if args.len() == 1 => Ok(ParsedCommand::PrintHelp(HelpCommand::Root)),
        "--version" | "-V" if args.len() == 1 => Ok(ParsedCommand::PrintVersion),
        "help" => parse_help(&args[1..]),
        "status" => parse_status(&args[1..]),
        "ports" => parse_ports(&args[1..]),
        "up" => parse_host_only("up", &args[1..]),
        "rebuild" => parse_host_only("rebuild", &args[1..]),
        "down" => parse_host_only("down", &args[1..]),
        "remove" => parse_host_only("remove", &args[1..]),
        "rm" => parse_host_only("rm", &args[1..]),
        "clean" => parse_host_only("clean", &args[1..]),
        option if option.starts_with('-') => Err(UsageError::new(format!(
            "unknown option for decune inside a container: {option}"
        ))),
        command => Err(UsageError::new(format!(
            "unknown decune command inside a container: {command}"
        ))),
    }
}

fn parse_help(args: &[&str]) -> Result<ParsedCommand, UsageError> {
    match args {
        [] => Ok(ParsedCommand::PrintHelp(HelpCommand::Root)),
        ["status"] => Ok(ParsedCommand::PrintHelp(HelpCommand::Status)),
        ["ports"] => Ok(ParsedCommand::PrintHelp(HelpCommand::Ports)),
        ["up"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("up"))),
        ["rebuild"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("rebuild"))),
        ["down"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("down"))),
        ["remove"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("remove"))),
        ["rm"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("rm"))),
        ["clean"] => Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly("clean"))),
        [command] => Err(UsageError::new(format!(
            "unknown decune help command inside a container: {command}"
        ))),
        _ => Err(UsageError::new(
            "decune help accepts at most one command inside a container",
        )),
    }
}

fn parse_status(args: &[&str]) -> Result<ParsedCommand, UsageError> {
    if args.is_empty() {
        return Ok(ParsedCommand::Query(Query {
            command: QueryCommand::Status,
            format: QueryFormat::Text,
        }));
    }
    if matches!(args, ["--help" | "-h"]) {
        return Ok(ParsedCommand::PrintHelp(HelpCommand::Status));
    }
    if args.contains(&"--json") {
        return Err(UsageError::new(
            "decune status --json is not supported inside a container",
        ));
    }
    if let Some(option) = args.iter().find(|argument| argument.starts_with('-')) {
        return Err(UsageError::new(format!(
            "unknown option for decune status inside a container: {option}"
        )));
    }
    Err(UsageError::new(
        "decune status does not accept a workspace argument inside a container; it always queries the current workspace",
    ))
}

fn parse_ports(args: &[&str]) -> Result<ParsedCommand, UsageError> {
    if args.is_empty() {
        return Ok(ParsedCommand::Query(Query {
            command: QueryCommand::Ports,
            format: QueryFormat::Text,
        }));
    }
    if matches!(args, ["--help" | "-h"]) {
        return Ok(ParsedCommand::PrintHelp(HelpCommand::Ports));
    }
    if args.contains(&"--all") {
        return Err(UsageError::new(
            "decune ports --all cannot be run inside a container; run it on the host",
        ));
    }
    if let Some(option) = args
        .iter()
        .find(|argument| argument.starts_with('-') && **argument != "--json")
    {
        return Err(UsageError::new(format!(
            "unknown option for decune ports inside a container: {option}"
        )));
    }
    if args.iter().any(|argument| !argument.starts_with('-')) {
        return Err(UsageError::new(
            "decune ports does not accept a workspace argument inside a container; it always queries the current workspace",
        ));
    }
    Ok(ParsedCommand::Query(Query {
        command: QueryCommand::Ports,
        format: QueryFormat::Json,
    }))
}

fn parse_host_only(command: &'static str, args: &[&str]) -> Result<ParsedCommand, UsageError> {
    if matches!(args, ["--help" | "-h"]) {
        return Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly(command)));
    }
    Err(UsageError::new(format!(
        "decune {command} cannot be run inside a container; run it on the host"
    )))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    use super::{HelpCommand, ParsedCommand, Query, QueryCommand, QueryFormat, UsageError, parse};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn command_matrix_is_table_driven() {
        let query = |command, format| Ok(ParsedCommand::Query(Query { command, format }));
        let error = |message: &str| {
            Err(UsageError {
                message: message.to_owned(),
                show_root_help: false,
            })
        };
        let cases = [
            (
                vec!["status"],
                query(QueryCommand::Status, QueryFormat::Text),
            ),
            (
                vec!["status", "--json"],
                error("decune status --json is not supported inside a container"),
            ),
            (
                vec!["status", "."],
                error(
                    "decune status does not accept a workspace argument inside a container; it always queries the current workspace",
                ),
            ),
            (
                vec!["status", "/workspace"],
                error(
                    "decune status does not accept a workspace argument inside a container; it always queries the current workspace",
                ),
            ),
            (
                vec!["status", ".", "--future"],
                error("unknown option for decune status inside a container: --future"),
            ),
            (vec!["ports"], query(QueryCommand::Ports, QueryFormat::Text)),
            (
                vec!["ports", "--json"],
                query(QueryCommand::Ports, QueryFormat::Json),
            ),
            (
                vec!["ports", "--all"],
                error("decune ports --all cannot be run inside a container; run it on the host"),
            ),
            (
                vec!["ports", "--all", "--json"],
                error("decune ports --all cannot be run inside a container; run it on the host"),
            ),
            (
                vec!["ports", "."],
                error(
                    "decune ports does not accept a workspace argument inside a container; it always queries the current workspace",
                ),
            ),
            (
                vec!["ports", ".", "--json"],
                error(
                    "decune ports does not accept a workspace argument inside a container; it always queries the current workspace",
                ),
            ),
            (
                vec!["inspect"],
                error("unknown decune command inside a container: inspect"),
            ),
            (
                vec!["--json"],
                error("unknown option for decune inside a container: --json"),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(parse(&args(&input)), expected, "input: {input:?}");
        }

        for command in ["up", "rebuild", "down", "remove", "rm", "clean"] {
            assert_eq!(
                parse(&args(&[command])),
                error(&format!(
                    "decune {command} cannot be run inside a container; run it on the host"
                )),
                "command: {command}"
            );
        }
    }

    #[test]
    fn help_version_and_aliases_are_local_commands() {
        for input in [vec!["--help"], vec!["-h"], vec!["help"]] {
            assert_eq!(
                parse(&args(&input)),
                Ok(ParsedCommand::PrintHelp(HelpCommand::Root))
            );
        }
        for input in [vec!["--version"], vec!["-V"]] {
            assert_eq!(parse(&args(&input)), Ok(ParsedCommand::PrintVersion));
        }
        for input in [vec!["status", "--help"], vec!["help", "status"]] {
            assert_eq!(
                parse(&args(&input)),
                Ok(ParsedCommand::PrintHelp(HelpCommand::Status))
            );
        }
        for input in [vec!["ports", "--help"], vec!["help", "ports"]] {
            assert_eq!(
                parse(&args(&input)),
                Ok(ParsedCommand::PrintHelp(HelpCommand::Ports))
            );
        }
        for command in ["up", "rebuild", "down", "remove", "rm", "clean"] {
            assert_eq!(
                parse(&args(&[command, "--help"])),
                Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly(command)))
            );
            assert_eq!(
                parse(&args(&["help", command])),
                Ok(ParsedCommand::PrintHelp(HelpCommand::HostOnly(command)))
            );
        }
    }

    #[test]
    fn missing_command_requests_root_help_with_usage_error() {
        assert_eq!(
            parse(&[]),
            Err(UsageError {
                message: "decune command is required inside a container".to_owned(),
                show_root_help: true,
            })
        );
    }

    #[test]
    fn non_utf8_argument_is_rejected_without_panicking() {
        let argument = OsString::from_vec(vec![b's', b't', b'a', b't', b'u', b's', 0xff]);

        assert_eq!(
            parse(&[argument]),
            Err(UsageError {
                message: "decune arguments must contain valid UTF-8 inside a container".to_owned(),
                show_root_help: false,
            })
        );
    }
}
