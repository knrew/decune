use std::{
    io::{self, Write},
    sync::OnceLock,
    time::Duration,
};

use console::{Style, style};
use indicatif::{ProgressBar, ProgressStyle};

use crate::terminal;

const PREFIX_WIDTH: usize = 12;

fn is_tty() -> bool {
    static TTY: OnceLock<bool> = OnceLock::new();
    *TTY.get_or_init(terminal::stderr_is_tty)
}

fn split_action(message: &str) -> (&str, &str) {
    message.split_once(' ').unwrap_or(("", message))
}

fn write_styled(action: &str, message: &str, action_style: &Style) {
    let styled_action = action_style.apply_to(format!("{action:>PREFIX_WIDTH$}"));
    let _ = writeln!(io::stderr().lock(), "{styled_action}  {message}");
}

fn write_plain(level: &str, message: &str) {
    let _ = writeln!(io::stderr().lock(), "{level}: {message}");
}

pub(crate) fn info(message: &str) {
    if is_tty() {
        let (action, detail) = split_action(message);
        if action.is_empty() {
            write_styled("Info", message, &Style::new().green().bold());
        } else {
            write_styled(action, detail, &Style::new().green().bold());
        }
    } else {
        write_plain("Info", message);
    }
}

pub(crate) fn status(action: &str, message: &str) {
    if is_tty() {
        write_styled(action, message, &Style::new().green().bold());
    } else {
        write_plain(action, message);
    }
}

pub(crate) fn skipped(message: &str) {
    if is_tty() {
        write_styled("Skipped", message, &Style::new().dim());
    } else {
        write_plain("Skipped", message);
    }
}

pub(crate) fn warn(message: &str) {
    if is_tty() {
        write_styled("Warning", message, &Style::new().yellow().bold());
    } else {
        write_plain("Warning", message);
    }
}

pub(crate) fn error(message: &str) {
    if is_tty() {
        write_styled("Error", message, &Style::new().red().bold());
    } else {
        write_plain("Error", message);
    }
}

pub(crate) fn done(message: &str) {
    if is_tty() {
        let (action, detail) = split_action(message);
        if action.is_empty() {
            write_styled("Done", message, &Style::new().green().bold());
        } else {
            let line = format!("{detail}  {}", style("✓").green());
            write_styled(action, &line, &Style::new().green().bold());
        }
    } else {
        write_plain("Done", message);
    }
}

pub(crate) fn finished(message: &str, elapsed: Duration) {
    let elapsed_display = format!("[{:.1}s]", elapsed.as_secs_f64());
    if is_tty() {
        let (action, detail) = split_action(message);
        let dimmed = style(&elapsed_display).dim();
        if action.is_empty() {
            let line = format!("{message}  {}", style("✓").green());
            write_styled(
                "Finished",
                &format!("{line} {dimmed}"),
                &Style::new().green().bold(),
            );
        } else {
            let line = format!("{detail}  {}", style("✓").green());
            write_styled(
                action,
                &format!("{line} {dimmed}"),
                &Style::new().green().bold(),
            );
        }
    } else {
        write_plain("Done", &format!("{message} {elapsed_display}"));
    }
}

pub(crate) fn spinner(message: &str) -> Spinner {
    if is_tty() {
        let (action, detail) = split_action(message);
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{prefix:>12.green.bold}  {msg} {spinner}")
                .expect("valid spinner template"),
        );
        if action.is_empty() {
            pb.set_prefix("Info");
            pb.set_message(message.to_owned());
        } else {
            pb.set_prefix(action.to_owned());
            pb.set_message(detail.to_owned());
        }
        pb.enable_steady_tick(Duration::from_millis(80));
        Spinner {
            inner: Some(SpinnerInner::Tty(pb)),
        }
    } else {
        write_plain("Info", message);
        Spinner {
            inner: Some(SpinnerInner::Plain),
        }
    }
}

pub(crate) struct Spinner {
    inner: Option<SpinnerInner>,
}

enum SpinnerInner {
    Tty(ProgressBar),
    Plain,
}

impl Spinner {
    pub(crate) fn finish(mut self, done_message: &str) {
        match self.inner.take() {
            Some(SpinnerInner::Tty(pb)) => {
                let (action, detail) = split_action(done_message);
                if action.is_empty() {
                    let done_line = format!("{done_message}  {}", style("✓").green());
                    pb.set_style(
                        ProgressStyle::default_spinner()
                            .template("{prefix:>12.green.bold}  {msg}")
                            .expect("valid template"),
                    );
                    pb.set_prefix("Done");
                    pb.finish_with_message(done_line);
                } else {
                    let done_line = format!("{detail}  {}", style("✓").green());
                    pb.set_style(
                        ProgressStyle::default_spinner()
                            .template("{prefix:>12.green.bold}  {msg}")
                            .expect("valid template"),
                    );
                    pb.set_prefix(action.to_owned());
                    pb.finish_with_message(done_line);
                }
            }
            Some(SpinnerInner::Plain) => {
                write_plain("Done", done_message);
            }
            None => {}
        }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    pub(crate) fn finish_quiet(mut self) {
        match self.inner.take() {
            Some(SpinnerInner::Tty(pb)) => pb.finish_and_clear(),
            Some(SpinnerInner::Plain) | None => {}
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        match &self.inner {
            Some(SpinnerInner::Tty(pb)) => pb.finish_and_clear(),
            Some(SpinnerInner::Plain) | None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::time::Duration;

    use indicatif::ProgressBar;

    use super::{Spinner, SpinnerInner, split_action};

    #[test]
    fn split_action_extracts_first_word() {
        assert_eq!(
            split_action("Pulling Docker image: foo"),
            ("Pulling", "Docker image: foo")
        );
    }

    #[test]
    fn split_action_handles_no_space() {
        assert_eq!(split_action("message"), ("", "message"));
    }

    #[test]
    fn tty_spinner_finish_drops_progress_bar() {
        let pb = ProgressBar::hidden();
        let weak = pb.downgrade();
        let spinner = Spinner {
            inner: Some(SpinnerInner::Tty(pb)),
        };

        spinner.finish("Done building");

        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn tty_spinner_finish_quiet_drops_progress_bar() {
        let pb = ProgressBar::hidden();
        let weak = pb.downgrade();
        let spinner = Spinner {
            inner: Some(SpinnerInner::Tty(pb)),
        };

        spinner.finish_quiet();

        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn unfinished_tty_spinner_drop_drops_progress_bar() {
        let weak = {
            let pb = ProgressBar::hidden();
            let weak = pb.downgrade();
            let _spinner = Spinner {
                inner: Some(SpinnerInner::Tty(pb)),
            };
            weak
        };

        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn plain_done_message_is_prefixed() {
        let message = "Started dev container: test";
        let mut output = Vec::new();
        writeln!(&mut output, "Done: {message}").unwrap();

        assert_eq!(output, b"Done: Started dev container: test\n");
    }

    #[test]
    fn plain_warning_message_is_prefixed() {
        let message = "Credential helper is unavailable";
        let mut output = Vec::new();
        writeln!(&mut output, "Warning: {message}").unwrap();

        assert_eq!(output, b"Warning: Credential helper is unavailable\n");
    }

    #[test]
    fn plain_finished_message_includes_elapsed() {
        let elapsed = Duration::from_secs_f64(12.3);
        let elapsed_display = format!("[{:.1}s]", elapsed.as_secs_f64());
        let message = "Started dev container: test";
        let mut output = Vec::new();
        writeln!(&mut output, "Done: {message} {elapsed_display}").unwrap();

        assert_eq!(output, b"Done: Started dev container: test [12.3s]\n");
    }

    #[test]
    fn plain_info_message_is_prefixed() {
        let level = "Info";
        let message = "Pulling Docker image: ubuntu";
        let mut output = Vec::new();
        writeln!(&mut output, "{level}: {message}").unwrap();
        assert_eq!(output, b"Info: Pulling Docker image: ubuntu\n");
    }

    #[test]
    fn plain_skipped_message_is_prefixed() {
        let message = "postCreateCommand";
        let mut output = Vec::new();
        writeln!(&mut output, "Skipped: {message}").unwrap();
        assert_eq!(output, b"Skipped: postCreateCommand\n");
    }
}
