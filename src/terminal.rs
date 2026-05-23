use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalSize {
    pub(crate) width: i32,
    pub(crate) height: i32,
}

pub(crate) struct RawTerminalGuard {
    #[cfg(unix)]
    fd: i32,
    #[cfg(unix)]
    original: Option<libc::termios>,
}

impl RawTerminalGuard {
    pub(crate) fn enter_stdin_if_tty() -> Result<Self> {
        #[cfg(unix)]
        {
            let fd = libc::STDIN_FILENO;
            if !is_tty(fd) {
                return Ok(Self { fd, original: None });
            }

            let original = termios_for_fd(fd)?;
            let mut raw = original;
            // SAFETY: cfmakeraw は有効な termios 構造体を受け取り，構造体内の flag だけを変更する．
            unsafe {
                libc::cfmakeraw(&mut raw);
            }
            set_termios(fd, &raw).context("Failed to enable raw terminal mode")?;

            Ok(Self {
                fd,
                original: Some(original),
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(original) = self.original {
            let _ = set_termios(self.fd, &original);
        }
    }
}

pub(crate) fn current_size() -> Option<TerminalSize> {
    #[cfg(unix)]
    {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        // SAFETY: ioctl は stdout fd と winsize 用の書き込み可能なポインタを受け取る．失敗時は戻り値で判定する．
        let status =
            unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };
        if status != 0 {
            return None;
        }

        // SAFETY: ioctl が成功した場合，winsize は kernel により初期化済みである．
        let size = unsafe { size.assume_init() };
        if size.ws_col == 0 || size.ws_row == 0 {
            return None;
        }

        Some(TerminalSize {
            width: i32::from(size.ws_col),
            height: i32::from(size.ws_row),
        })
    }

    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn is_tty(fd: i32) -> bool {
    // SAFETY: isatty は fd を読み取るだけで，失敗は 0 として返る．
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(unix)]
fn termios_for_fd(fd: i32) -> Result<libc::termios> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: tcgetattr は成功時に termios ポインタへ初期化済み値を書き込む．
    let status = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("Failed to read terminal attributes");
    }

    // SAFETY: tcgetattr が成功した場合，termios は初期化済みである．
    Ok(unsafe { termios.assume_init() })
}

#[cfg(unix)]
fn set_termios(fd: i32, termios: &libc::termios) -> Result<()> {
    // SAFETY: tcsetattr は fd と termios 参照を読み取る．失敗は戻り値で判定する．
    let status = unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) };
    if status != 0 {
        return Err(std::io::Error::last_os_error()).context("Failed to write terminal attributes");
    }

    Ok(())
}
