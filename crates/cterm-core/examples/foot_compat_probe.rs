//! PTY-side probe used by the Linux cterm-versus-foot differential CI gate.

#[cfg(not(unix))]
fn main() {
    eprintln!("foot_compat_probe is only supported on Unix");
    std::process::exit(2);
}

#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    unix::run()
}

#[cfg(unix)]
mod unix {
    use std::env;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    struct RawMode {
        fd: libc::c_int,
        original: libc::termios,
    }

    impl RawMode {
        fn enable(fd: libc::c_int) -> io::Result<Self> {
            let mut original = unsafe { std::mem::zeroed() };
            if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut raw = original;
            unsafe { libc::cfmakeraw(&mut raw) };
            if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { fd, original })
        }
    }

    impl Drop for RawMode {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
            }
        }
    }

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let output_path = env::args_os().nth(1).ok_or("missing output path")?;
        let stdin = io::stdin();
        let _raw_mode = RawMode::enable(stdin.as_raw_fd())?;
        let mut stdin = stdin.lock();
        let mut stdout = io::stdout().lock();

        let cases: &[(&str, &[u8], &[u8])] = &[
            ("device-attributes", b"\x1b[c", b"c"),
            ("cursor", b"\x1b[2J\x1b[H\x1b[6;11H\x1b[6n", b"R"),
            (
                "sgr",
                b"\x1b[1;3;4:3;38:2::1:2:3;48:5:42;58:2::4:5:6m\x1bP$qm\x1b\\",
                b"\x1b\\",
            ),
            ("margins", b"\x1b[3;20r\x1bP$qr\x1b\\", b"\x1b\\"),
            ("cursor-style", b"\x1b[5 q\x1bP$q q\x1b\\", b"\x1b\\"),
            ("sync-mode", b"\x1b[?2026$p", b"y"),
            ("reverse-wrap", b"\x1b[?45$p", b"y"),
            ("terminfo-am", b"\x1bP+q616d\x1b\\", b"\x1b\\"),
            (
                "palette-stack",
                b"\x1b]10;#112233\x1b\\\x1b[#P\x1b]10;#445566\x1b\\\x1b[#Q\x1b]10;?\x1b\\",
                b"\x1b\\",
            ),
            ("palette-stack-status", b"\x1b[#P\x1b[#R\x1b[#Q", b"#Q"),
            ("theme", b"\x1b[?996n", b"n"),
            ("visibility", b"\x1b[?998n", b"n"),
            ("theme-report-mode", b"\x1b[?2031h\x1b[?2031$p", b"y"),
            ("visibility-report-mode", b"\x1b[?2033h", b"n"),
            (
                "visibility-report-mode-reset",
                b"\x1b[?2033l\x1b[?2033$p",
                b"y",
            ),
        ];

        let mut report = String::new();
        for (name, request, terminator) in cases {
            eprintln!("cterm compatibility probe: starting {name}");
            let response = exchange(&mut stdin, &mut stdout, request, terminator)?;
            report.push_str(name);
            report.push('=');
            for byte in response {
                use std::fmt::Write as _;
                write!(report, "{byte:02x}")?;
            }
            report.push('\n');
            fs::write(&output_path, &report)?;
            eprintln!("cterm compatibility probe: completed {name}");
        }

        stdout.write_all(b"\x1b[0m\x1b[r\x1b[H\x1b]110\x1b\\")?;
        stdout.flush()?;
        fs::write(output_path, report)?;
        Ok(())
    }

    fn exchange(
        input: &mut impl Read,
        output: &mut impl Write,
        request: &[u8],
        terminator: &[u8],
    ) -> io::Result<Vec<u8>> {
        output.write_all(request)?;
        output.flush()?;

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut response = Vec::new();
        let mut buffer = [0_u8; 256];
        loop {
            if response.ends_with(terminator) {
                return Ok(response);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("terminal reply timed out after {} bytes", response.len()),
                ));
            }
            let timeout = remaining.as_millis().min(libc::c_int::MAX as u128) as libc::c_int;
            let mut poll_fd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
            if ready < 0 {
                return Err(io::Error::last_os_error());
            }
            if ready == 0 {
                continue;
            }
            let read = input.read(&mut buffer)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal closed before replying",
                ));
            }
            response.extend_from_slice(&buffer[..read]);
        }
    }
}
