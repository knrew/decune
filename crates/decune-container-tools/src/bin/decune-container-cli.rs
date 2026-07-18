use std::{
    env,
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[path = "decune_container_cli/parser.rs"]
mod parser;
#[path = "decune_container_cli/transport.rs"]
mod transport;

use parser::{ParsedCommand, Query, ROOT_HELP};
use transport::{
    INVALID_RESPONSE_MESSAGE, QueryError, QuerySuccess, TRANSPORT_ERROR_MESSAGE,
    UNAVAILABLE_MESSAGE,
};

const HOST_DAEMON_SOCKET_TARGET: &str = "/run/decune/host-daemon.sock";
const HOST_DAEMON_SOCKET_ENV: &str = "DECUNE_HOST_DAEMON_SOCKET";

const EXIT_SUCCESS: u8 = 0;
const EXIT_FAILURE: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> std::process::ExitCode {
    let args = env::args_os().skip(1).collect::<Vec<_>>();
    let socket_path = env::var_os(HOST_DAEMON_SOCKET_ENV)
        .map_or_else(|| PathBuf::from(HOST_DAEMON_SOCKET_TARGET), PathBuf::from);
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    std::process::ExitCode::from(execute(&args, &socket_path, &mut stdout, &mut stderr))
}

fn execute(
    args: &[OsString],
    socket_path: &Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> u8 {
    execute_with(args, socket_path, stdout, stderr, transport::query)
}

fn execute_with(
    args: &[OsString],
    socket_path: &Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
    mut query_daemon: impl FnMut(&Path, Query) -> Result<QuerySuccess, QueryError>,
) -> u8 {
    let parsed = match parser::parse(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            if write_prefixed_line(stderr, "Error: ", &error.message).is_err() {
                return EXIT_FAILURE;
            }
            if error.show_root_help
                && (stderr.write_all(b"\n").is_err()
                    || stderr.write_all(ROOT_HELP.as_bytes()).is_err())
            {
                return EXIT_FAILURE;
            }
            return EXIT_USAGE;
        }
    };

    match parsed {
        ParsedCommand::PrintHelp(command) => {
            if stdout.write_all(command.text().as_bytes()).is_ok() {
                EXIT_SUCCESS
            } else {
                EXIT_FAILURE
            }
        }
        ParsedCommand::PrintVersion => {
            if writeln!(stdout, "decune {}", env!("CARGO_PKG_VERSION")).is_ok() {
                EXIT_SUCCESS
            } else {
                EXIT_FAILURE
            }
        }
        ParsedCommand::Query(query) => match query_daemon(socket_path, query) {
            Ok(success) => {
                for warning in success.warnings {
                    if write_prefixed_line(stderr, "Warning: ", &warning).is_err() {
                        return EXIT_FAILURE;
                    }
                }
                if stdout.write_all(success.output.as_bytes()).is_ok() {
                    EXIT_SUCCESS
                } else {
                    EXIT_FAILURE
                }
            }
            Err(error) => write_query_error(stderr, error),
        },
    }
}

fn write_query_error(stderr: &mut impl Write, error: QueryError) -> u8 {
    let message = match error {
        QueryError::Unavailable => UNAVAILABLE_MESSAGE,
        QueryError::Transport => TRANSPORT_ERROR_MESSAGE,
        QueryError::InvalidResponse => INVALID_RESPONSE_MESSAGE,
        QueryError::Daemon(message) => {
            _ = write_prefixed_line(stderr, "Error: ", &message);
            return EXIT_FAILURE;
        }
    };
    _ = write_prefixed_line(stderr, "Error: ", message);
    EXIT_FAILURE
}

fn write_prefixed_line(writer: &mut impl Write, prefix: &str, message: &str) -> io::Result<()> {
    writer.write_all(prefix.as_bytes())?;
    writer.write_all(message.trim_end_matches(['\r', '\n']).as_bytes())?;
    writer.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, ffi::OsString, path::Path};

    use super::{
        EXIT_FAILURE, EXIT_SUCCESS, EXIT_USAGE, QueryError, QuerySuccess, execute, execute_with,
        write_prefixed_line,
    };
    use crate::parser::{Query, QueryCommand, QueryFormat};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn local_commands_and_usage_errors_do_not_connect_to_socket() {
        let socket_path = Path::new("/path/that/must/not/be/used");
        let cases = [
            (vec!["--help"], EXIT_SUCCESS),
            (vec!["--help", "status"], EXIT_SUCCESS),
            (vec!["--version"], EXIT_SUCCESS),
            (vec!["status", "--json"], EXIT_USAGE),
            (vec!["status", "."], EXIT_USAGE),
            (vec!["ports", "--all"], EXIT_USAGE),
            (vec!["ports", "."], EXIT_USAGE),
            (vec!["ports", "--json", "--json"], EXIT_USAGE),
            (vec!["ports", "--json", "--help"], EXIT_SUCCESS),
            (vec!["up"], EXIT_USAGE),
            (vec!["unknown"], EXIT_USAGE),
        ];

        for (input, expected_exit) in cases {
            let connections = Cell::new(0);
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = execute_with(
                &args(&input),
                socket_path,
                &mut stdout,
                &mut stderr,
                |_, _| {
                    connections.set(connections.get() + 1);
                    Err(QueryError::Transport)
                },
            );

            assert_eq!(exit, expected_exit, "input: {input:?}");
            assert_eq!(connections.get(), 0, "input: {input:?}");
        }
    }

    #[test]
    fn missing_command_prints_error_and_root_help_to_stderr() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute(&[], Path::new("/unused"), &mut stdout, &mut stderr);

        assert_eq!(exit, EXIT_USAGE);
        assert!(stdout.is_empty());
        let stderr = String::from_utf8(stderr).unwrap();
        assert!(stderr.starts_with(
            "Error: decune command is required inside a container\n\nRun decune queries"
        ));
        assert!(stderr.contains("Usage: decune <COMMAND>"));
    }

    #[test]
    fn help_version_and_host_only_help_write_local_stdout() {
        let cases = [
            (vec!["--help"], "Usage: decune <COMMAND>"),
            (vec!["--help", "status"], "Usage: decune <COMMAND>"),
            (vec!["help", "status"], "Usage: decune status"),
            (vec!["ports", "--help"], "Usage: decune ports [--json]"),
            (
                vec!["ports", "--json", "--help"],
                "Usage: decune ports [--json]",
            ),
            (
                vec!["help", "up"],
                "`decune up` can only be run on the host.",
            ),
            (
                vec!["--version"],
                concat!("decune ", env!("CARGO_PKG_VERSION")),
            ),
        ];

        for (input, expected) in cases {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let exit = execute_with(
                &args(&input),
                Path::new("/unused"),
                &mut stdout,
                &mut stderr,
                |_, _| panic!("local output must not query the daemon"),
            );

            assert_eq!(exit, EXIT_SUCCESS, "input: {input:?}");
            assert!(
                String::from_utf8(stdout).unwrap().contains(expected),
                "input: {input:?}"
            );
            assert!(stderr.is_empty(), "input: {input:?}");
        }
    }

    #[test]
    fn duplicate_ports_json_is_a_usage_error_without_daemon_output() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["ports", "--json", "--json"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, _| panic!("usage errors must not query the daemon"),
        );

        assert_eq!(exit, EXIT_USAGE);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            b"Error: decune ports --json cannot be used more than once inside a container\n"
        );
    }

    #[test]
    fn success_writes_warnings_to_stderr_and_unmodified_output_to_stdout() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["status"]),
            Path::new("/test/socket"),
            &mut stdout,
            &mut stderr,
            |path, query| {
                assert_eq!(path, Path::new("/test/socket"));
                assert_eq!(
                    query,
                    Query {
                        command: QueryCommand::Status,
                        format: QueryFormat::Text,
                    }
                );
                Ok(QuerySuccess {
                    output: "output without newline".to_owned(),
                    warnings: vec!["first\n".to_owned(), "second".to_owned()],
                })
            },
        );

        assert_eq!(exit, EXIT_SUCCESS);
        assert_eq!(stdout, b"output without newline");
        assert_eq!(stderr, b"Warning: first\nWarning: second\n");
    }

    #[test]
    fn ports_json_stdout_contains_only_daemon_output() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["ports", "--json"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, query| {
                assert_eq!(
                    query,
                    Query {
                        command: QueryCommand::Ports,
                        format: QueryFormat::Json,
                    }
                );
                Ok(QuerySuccess {
                    output: r#"{"ports":[]}"#.to_owned(),
                    warnings: vec!["stale".to_owned()],
                })
            },
        );

        assert_eq!(exit, EXIT_SUCCESS);
        assert_eq!(stdout, br#"{"ports":[]}"#);
        assert_eq!(stderr, b"Warning: stale\n");
    }

    #[test]
    fn daemon_error_has_empty_stdout_and_single_error_line() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["ports"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, _| Err(QueryError::Daemon("daemon failed\n".to_owned())),
        );

        assert_eq!(exit, EXIT_FAILURE);
        assert!(stdout.is_empty());
        assert_eq!(stderr, b"Error: daemon failed\n");
    }

    #[test]
    fn unavailable_socket_uses_canonical_message() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["status"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, _| Err(QueryError::Unavailable),
        );

        assert_eq!(exit, EXIT_FAILURE);
        assert!(stdout.is_empty());
        assert_eq!(
            String::from_utf8(stderr).unwrap(),
            "Error: decune host daemon is unavailable; keep an active attached \"decune up\" session running on the host (detached mode is not supported)\n"
        );
    }

    #[test]
    fn invalid_response_has_empty_stdout_and_generic_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["status"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, _| Err(QueryError::InvalidResponse),
        );

        assert_eq!(exit, EXIT_FAILURE);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            b"Error: decune received an invalid response from the host daemon\n"
        );
    }

    #[test]
    fn transport_error_has_empty_stdout_and_generic_error() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        let exit = execute_with(
            &args(&["status"]),
            Path::new("/unused"),
            &mut stdout,
            &mut stderr,
            |_, _| Err(QueryError::Transport),
        );

        assert_eq!(exit, EXIT_FAILURE);
        assert!(stdout.is_empty());
        assert_eq!(
            stderr,
            b"Error: decune could not communicate with the host daemon\n"
        );
    }

    #[test]
    fn prefixed_line_removes_only_duplicate_trailing_newlines() {
        let mut output = Vec::new();

        write_prefixed_line(&mut output, "Warning: ", "first line\nsecond line\r\n\n").unwrap();

        assert_eq!(output, b"Warning: first line\nsecond line\n");
    }
}
