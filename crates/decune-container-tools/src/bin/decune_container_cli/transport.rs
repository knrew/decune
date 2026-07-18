use std::{
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    thread,
    time::Duration,
};

use decune_container_protocol::{
    CliQueryRequest, HOST_DAEMON_PROTOCOL_VERSION, HostDaemonResponse, REQUEST_TYPE_CLI_QUERY,
};

use super::parser::Query;

const CONNECT_ATTEMPTS: usize = 5;
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

pub const UNAVAILABLE_MESSAGE: &str = "decune host daemon is unavailable; keep an active attached \"decune up\" session running on the host (detached mode is not supported)";
pub const TRANSPORT_ERROR_MESSAGE: &str = "decune could not communicate with the host daemon";
pub const INVALID_RESPONSE_MESSAGE: &str =
    "decune received an invalid response from the host daemon";

#[derive(Debug, PartialEq, Eq)]
pub struct QuerySuccess {
    pub output: String,
    pub warnings: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum QueryError {
    Unavailable,
    Transport,
    InvalidResponse,
    Daemon(String),
}

pub fn query(socket_path: &Path, query: Query) -> Result<QuerySuccess, QueryError> {
    query_with(
        socket_path,
        query,
        |path| UnixStream::connect(path),
        thread::sleep,
    )
}

pub fn query_with(
    socket_path: &Path,
    query: Query,
    mut connect: impl FnMut(&Path) -> io::Result<UnixStream>,
    mut sleep: impl FnMut(Duration),
) -> Result<QuerySuccess, QueryError> {
    let request = serde_json::to_vec(&CliQueryRequest {
        version: HOST_DAEMON_PROTOCOL_VERSION,
        request_type: REQUEST_TYPE_CLI_QUERY.to_owned(),
        command: query.command.as_str().to_owned(),
        format: query.format.as_str().to_owned(),
    })
    .map_err(|_error| QueryError::Transport)?;

    let mut stream = connect_with_retry(socket_path, &mut connect, &mut sleep)?;
    stream
        .write_all(&request)
        .map_err(|_error| QueryError::Transport)?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|_error| QueryError::Transport)?;

    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|_error| QueryError::Transport)?;
    parse_response(&response)
}

fn connect_with_retry(
    socket_path: &Path,
    connect: &mut impl FnMut(&Path) -> io::Result<UnixStream>,
    sleep: &mut impl FnMut(Duration),
) -> Result<UnixStream, QueryError> {
    for attempt in 0..CONNECT_ATTEMPTS {
        match connect(socket_path) {
            Ok(stream) => return Ok(stream),
            Err(error) if is_handoff_error(&error) => {
                if attempt + 1 == CONNECT_ATTEMPTS {
                    return Err(QueryError::Unavailable);
                }
                sleep(RETRY_INTERVAL);
            }
            Err(_) => return Err(QueryError::Transport),
        }
    }
    unreachable!("connect retry loop always returns on its final attempt")
}

fn is_handoff_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    )
}

fn parse_response(bytes: &[u8]) -> Result<QuerySuccess, QueryError> {
    let response: HostDaemonResponse =
        serde_json::from_slice(bytes).map_err(|_error| QueryError::InvalidResponse)?;
    if response.version != HOST_DAEMON_PROTOCOL_VERSION {
        return Err(QueryError::InvalidResponse);
    }

    let warnings = response.warnings.clone();
    match response.into_result() {
        Ok(Ok(output)) => Ok(QuerySuccess { output, warnings }),
        Ok(Err(error)) => Err(QueryError::Daemon(error.message)),
        Err(_) => Err(QueryError::InvalidResponse),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        io::{self, Read, Write},
        net::Shutdown,
        os::unix::net::UnixStream,
        path::Path,
        thread,
        time::Duration,
    };

    use super::{QueryError, QuerySuccess, query_with};
    use crate::parser::{Query, QueryCommand, QueryFormat};

    const STATUS_QUERY: Query = Query {
        command: QueryCommand::Status,
        format: QueryFormat::Text,
    };

    fn response_stream(response: &'static [u8]) -> UnixStream {
        let (client, mut server) = UnixStream::pair().unwrap();
        thread::spawn(move || {
            let mut request = Vec::new();
            server.read_to_end(&mut request).unwrap();
            server.write_all(response).unwrap();
            server.shutdown(Shutdown::Write).unwrap();
        });
        client
    }

    #[test]
    fn request_is_serialized_and_write_half_is_closed_before_response_read() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let server = thread::spawn(move || {
            let mut request = Vec::new();
            server.read_to_end(&mut request).unwrap();
            server
                .write_all(br#"{"version":1,"ok":true,"output":"ready"}"#)
                .unwrap();
            request
        });
        let mut client = Some(client);

        let result = query_with(
            Path::new("/unused"),
            STATUS_QUERY,
            |_| Ok(client.take().unwrap()),
            |_| {},
        )
        .unwrap();
        let request: serde_json::Value = serde_json::from_slice(&server.join().unwrap()).unwrap();

        assert_eq!(result.output, "ready");
        assert_eq!(
            request,
            serde_json::json!({
                "version": 1,
                "type": "cliQuery",
                "command": "status",
                "format": "text",
            })
        );
    }

    #[test]
    fn success_preserves_output_and_warning_order() {
        let result = query_with(
            Path::new("/unused"),
            STATUS_QUERY,
            |_| {
                Ok(response_stream(
                    br#"{"version":1,"ok":true,"output":"no trailing newline","warnings":["first","second"]}"#,
                ))
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(
            result,
            QuerySuccess {
                output: "no trailing newline".to_owned(),
                warnings: vec!["first".to_owned(), "second".to_owned()],
            }
        );
    }

    #[test]
    fn daemon_error_preserves_message_for_known_and_future_codes() {
        for response in [
            br#"{"version":1,"ok":false,"error":{"code":"cli_query_failed","message":"query failed"}}"#
                .as_slice(),
            br#"{"version":1,"ok":false,"error":{"code":"future_error","message":"future failure"}}"#
                .as_slice(),
        ] {
            let result = query_with(
                Path::new("/unused"),
                STATUS_QUERY,
                |_| Ok(response_stream(response)),
                |_| {},
            );

            let expected = if response.windows(b"future".len()).any(|part| part == b"future") {
                "future failure"
            } else {
                "query failed"
            };
            assert_eq!(result, Err(QueryError::Daemon(expected.to_owned())));
        }
    }

    #[test]
    fn malformed_responses_are_rejected() {
        for response in [
            b"".as_slice(),
            b"not json".as_slice(),
            br#"{"version":2,"ok":true,"output":"wrong protocol"}"#.as_slice(),
            br#"{"version":1,"ok":true}"#.as_slice(),
            br#"{"version":1,"ok":false,"error":{"code":"failed","message":"failed"},"warnings":["invalid"]}"#
                .as_slice(),
        ] {
            assert_eq!(
                query_with(
                    Path::new("/unused"),
                    STATUS_QUERY,
                    |_| Ok(response_stream(response)),
                    |_| {},
                ),
                Err(QueryError::InvalidResponse)
            );
        }
    }

    #[test]
    fn handoff_retry_is_bounded_to_five_attempts_and_four_intervals() {
        let attempts = Cell::new(0);
        let sleeps = RefCell::new(Vec::new());

        let result = query_with(
            Path::new("/unused"),
            STATUS_QUERY,
            |_| {
                attempts.set(attempts.get() + 1);
                Err(io::Error::from(io::ErrorKind::NotFound))
            },
            |duration| sleeps.borrow_mut().push(duration),
        );

        assert_eq!(result, Err(QueryError::Unavailable));
        assert_eq!(attempts.get(), 5);
        assert_eq!(sleeps.into_inner(), vec![Duration::from_millis(100); 4]);
    }

    #[test]
    fn handoff_retry_accepts_socket_that_appears() {
        let attempts = Cell::new(0);
        let sleeps = RefCell::new(Vec::new());
        let mut connections = VecDeque::from([
            Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
            Err(io::Error::from(io::ErrorKind::NotFound)),
            Ok(response_stream(
                br#"{"version":1,"ok":true,"output":"ready"}"#,
            )),
        ]);

        let result = query_with(
            Path::new("/unused"),
            STATUS_QUERY,
            |_| {
                attempts.set(attempts.get() + 1);
                connections.pop_front().unwrap()
            },
            |duration| sleeps.borrow_mut().push(duration),
        )
        .unwrap();

        assert_eq!(result.output, "ready");
        assert_eq!(attempts.get(), 3);
        assert_eq!(sleeps.into_inner(), vec![Duration::from_millis(100); 2]);
    }

    #[test]
    fn permission_and_other_transport_errors_are_not_retried() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::ConnectionReset,
        ] {
            let attempts = Cell::new(0);
            let result = query_with(
                Path::new("/unused"),
                STATUS_QUERY,
                |_| {
                    attempts.set(attempts.get() + 1);
                    Err(io::Error::from(kind))
                },
                |_| panic!("non-handoff errors must not sleep"),
            );

            assert_eq!(result, Err(QueryError::Transport));
            assert_eq!(attempts.get(), 1);
        }
    }
}
