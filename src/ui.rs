use std::io::{self, Write};

pub(crate) fn info(message: &str) {
    write_stderr("Info", message);
}

pub(crate) fn warn(message: &str) {
    write_stderr("Warning", message);
}

pub(crate) fn error(message: &str) {
    write_stderr("Error", message);
}

pub(crate) fn done(message: &str) {
    write_stderr("Done", message);
}

fn write_stderr(level: &str, message: &str) {
    let mut stderr = io::stderr().lock();
    let _ = write_prefixed(&mut stderr, level, message);
}

fn write_prefixed(writer: &mut impl Write, level: &str, message: &str) -> io::Result<()> {
    write_line(writer, &format!("{level}: {message}"))
}

fn write_line(writer: &mut impl Write, message: &str) -> io::Result<()> {
    writeln!(writer, "{message}")
}

#[cfg(test)]
mod tests {
    use super::{write_line, write_prefixed};

    #[test]
    fn prefixed_message_is_single_line() {
        let mut output = Vec::new();

        write_prefixed(&mut output, "Warning", "Credential helper is unavailable").unwrap();

        assert_eq!(output, b"Warning: Credential helper is unavailable\n");
    }

    #[test]
    fn value_message_has_no_prefix() {
        let mut output = Vec::new();

        write_line(&mut output, "/workspace").unwrap();

        assert_eq!(output, b"/workspace\n");
    }
}
