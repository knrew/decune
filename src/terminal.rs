pub(crate) fn stdin_is_tty() -> bool {
    #[cfg(unix)]
    {
        is_tty(libc::STDIN_FILENO)
    }

    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn stdout_is_tty() -> bool {
    #[cfg(unix)]
    {
        is_tty(libc::STDOUT_FILENO)
    }

    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn stderr_is_tty() -> bool {
    #[cfg(unix)]
    {
        is_tty(libc::STDERR_FILENO)
    }

    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether stdin and stdout of this process are terminals, which decides whether a
/// `docker exec` attached to them gets a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StdioTerminals {
    pub(crate) stdin: bool,
    pub(crate) stdout: bool,
}

impl StdioTerminals {
    pub(crate) fn detect() -> Self {
        Self {
            stdin: stdin_is_tty(),
            stdout: stdout_is_tty(),
        }
    }

    /// The `up` shell attach allocates a TTY whenever stdin is a terminal.
    pub(crate) const fn allocates_shell_tty(self) -> bool {
        self.stdin
    }

    /// `decune exec` allocates a TTY only when both stdin and stdout are terminals.
    ///
    /// With a TTY, `docker exec` merges the command's stderr into stdout and rewrites line
    /// endings, and it fails when stdin is not a terminal, so piped input or output would
    /// break.
    pub(crate) const fn allocates_exec_tty(self) -> bool {
        self.stdin && self.stdout
    }
}

#[cfg(unix)]
fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only reads the file descriptor, and failures are returned as 0.
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(test)]
mod tests {
    use super::StdioTerminals;

    // `decune exec` allocates a TTY only when both stdin and stdout are terminals.
    #[test]
    fn exec_allocates_tty_only_when_stdin_and_stdout_are_terminals() {
        let cases = [
            (true, true, true),
            (true, false, false),
            (false, true, false),
            (false, false, false),
        ];

        for (stdin, stdout, expected) in cases {
            assert_eq!(
                StdioTerminals { stdin, stdout }.allocates_exec_tty(),
                expected,
                "stdin: {stdin}, stdout: {stdout}"
            );
        }
    }

    // The `up` shell attach allocates a TTY whenever stdin is a terminal, whatever stdout is.
    #[test]
    fn shell_attach_allocates_tty_from_stdin_alone() {
        let cases = [
            (true, true, true),
            (true, false, true),
            (false, true, false),
            (false, false, false),
        ];

        for (stdin, stdout, expected) in cases {
            assert_eq!(
                StdioTerminals { stdin, stdout }.allocates_shell_tty(),
                expected,
                "stdin: {stdin}, stdout: {stdout}"
            );
        }
    }
}
