//! Bounded cloud-init subprocess capture without inherited-pipe waits.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub(super) fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let result = capture_child(&mut child, timeout, limit);
    // The child may already have exited while descendants still own stdout.
    // Its separate process group lets cleanup cover both cases without EOF waits.
    if let Ok(pid) = i32::try_from(child.id()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn capture_child(child: &mut Child, timeout: Duration, limit: usize) -> io::Result<Vec<u8>> {
    let started = Instant::now();
    let mut pipe = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("stdout pipe missing"))?;
    let flags = nix::fcntl::fcntl(pipe.as_raw_fd(), nix::fcntl::FcntlArg::F_GETFL)
        .map_err(io::Error::from)?;
    nix::fcntl::fcntl(
        pipe.as_raw_fd(),
        nix::fcntl::FcntlArg::F_SETFL(
            nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK,
        ),
    )
    .map_err(io::Error::from)?;
    let mut output = Vec::new();
    loop {
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "cloud-init query timed out",
            ));
        }
        let status = child.try_wait()?;
        // A finite drain budget keeps deadline checks reachable for chatty children.
        let mut chunk = [0u8; 8192];
        let mut drained = false;
        for _ in 0..16 {
            match pipe.read(&mut chunk) {
                Ok(0) => {
                    drained = true;
                    break;
                }
                Ok(n) => {
                    if n > limit.saturating_sub(output.len()) {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "cloud-init output exceeds capture limit",
                        ));
                    }
                    output.extend_from_slice(&chunk[..n]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if let Some(status) = status {
            if !drained {
                continue;
            }
            return if status.success() {
                Ok(output)
            } else {
                Err(io::Error::other(format!("cloud-init exited with {status}")))
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn retains_successful_output_and_rejects_exit_failure() {
        assert_eq!(
            capture(
                &mut shell("printf '{\"ok\":true}'"),
                Duration::from_secs(1),
                100
            )
            .unwrap(),
            b"{\"ok\":true}"
        );
        assert!(capture(&mut shell("exit 1"), Duration::from_secs(1), 100).is_err());
    }

    #[test]
    fn rejects_oversized_output_instead_of_parsing_a_prefix() {
        let error = capture(&mut shell("printf 123456"), Duration::from_secs(1), 5).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            capture(&mut shell("printf 12345"), Duration::from_secs(1), 5).unwrap(),
            b"12345"
        );
    }

    #[test]
    fn times_out_and_reaps_a_stuck_child() {
        let started = Instant::now();
        let error =
            capture(&mut shell("exec sleep 10"), Duration::from_millis(75), 100).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cleans_up_descendants_without_waiting_for_pipe_eof() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant-ran");
        let mut command = shell("(sleep 1; printf leaked > \"$MARKER\") & printf done");
        command.env("MARKER", &marker);
        let started = Instant::now();
        assert_eq!(
            capture(&mut command, Duration::from_secs(1), 100).unwrap(),
            b"done"
        );
        assert!(started.elapsed() < Duration::from_millis(750));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!marker.exists());
    }

    #[test]
    fn successful_large_output_is_drained_completely() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("large-json");
        let bytes = vec![b'x'; 256 * 1024];
        std::fs::write(&input, &bytes).unwrap();
        let mut command = shell("cat \"$INPUT\"");
        command.env("INPUT", &input);
        assert_eq!(
            capture(&mut command, Duration::from_secs(2), bytes.len()).unwrap(),
            bytes
        );
    }

    #[test]
    fn chatty_child_cannot_starve_the_deadline() {
        let error = capture(
            &mut shell("while :; do printf 1234567890; done"),
            Duration::from_millis(75),
            usize::MAX,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
