//! Bounded command execution for device utilities; no reader threads or orphaned children.
use std::{
    io::{self, Read},
    os::fd::AsRawFd,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn output(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = (|| {
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let mut stderr = child.stderr.take().expect("stderr is piped");
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            // Pipes are private to this invocation. Nonblocking reads let the same
            // thread drain both streams and enforce a deadline even if a child hangs.
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags == -1 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "device command timed out",
                ));
            }
            drain(&mut stdout, &mut out)?;
            drain(&mut stderr, &mut err)?;
            if let Some(status) = child.try_wait()? {
                drain(&mut stdout, &mut out)?;
                drain(&mut stderr, &mut err)?;
                return Ok(Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            thread::sleep(Duration::from_millis(10));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn drain(reader: &mut impl Read, captured: &mut Vec<u8>) -> io::Result<()> {
    const MAX_CAPTURE: usize = 64 * 1024;
    let mut buffer = [0; 4096];
    // Bound work per pass as well as memory, including for a noisy subprocess.
    for _ in 0..32 {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(MAX_CAPTURE.saturating_sub(captured.len()));
                captured.extend_from_slice(&buffer[..keep]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hung_command_is_killed_at_deadline() {
        let started = Instant::now();
        let error = output(
            Command::new("/bin/sleep").arg("10"),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn captures_both_streams_and_exit_status() {
        let result = output(
            Command::new("/bin/sh").args(["-c", "printf out; printf err >&2; exit 7"]),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(result.stdout, b"out");
        assert_eq!(result.stderr, b"err");
        assert_eq!(result.status.code(), Some(7));
    }
}
