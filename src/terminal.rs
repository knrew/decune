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

#[cfg(unix)]
fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty only reads the file descriptor, and failures are returned as 0.
    unsafe { libc::isatty(fd) == 1 }
}
