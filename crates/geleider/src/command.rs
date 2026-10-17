//! Running the helper programs geleider depends on (git, gh, codex) with a
//! deadline, so one that hangs cannot stall the thread that started it.

use std::io::{Read, Write};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

/// How often a running command is checked for having exited.
const POLL: Duration = Duration::from_millis(10);

/// Runs `command` with `input` on stdin, collecting its output, and kills it if
/// it is still running after `timeout`. Output is read while it runs, so a
/// command that prints a lot cannot block on a full pipe. (A process it starts
/// that keeps the pipe open after it exits would still hold the read.)
pub fn output_within(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Output> {
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        // Written from another thread so a command that never reads cannot block
        // us; the pipe closes when the thread ends.
        let input = input.to_vec();
        std::thread::spawn(move || stdin.write_all(&input));
    }
    let stdout = read_all(child.stdout.take());
    let stderr = read_all(child.stderr.take());
    let status = wait_within(&mut child, timeout)
        .with_context(|| format!("{program} did not finish within {}s", timeout.as_secs()))?;
    Ok(Output {
        status,
        stdout: joined(stdout)?,
        stderr: joined(stderr)?,
    })
}

/// Waits for `child` to exit, killing it once `timeout` has passed.
pub fn wait_within(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out");
        }
        std::thread::sleep(POLL);
    }
}

fn read_all(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<std::io::Result<Vec<u8>>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut bytes)?;
        }
        Ok(bytes)
    })
}

fn joined(reader: JoinHandle<std::io::Result<Vec<u8>>>) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow!("reading a command's output failed"))?
        .context("reading a command's output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_and_input_pass_through() {
        let out = output_within(
            &mut Command::new("cat"),
            Some(b"hello"),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }

    #[test]
    fn a_large_output_does_not_block_on_the_pipe() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 1000000 /dev/zero"]);
        let out = output_within(&mut cmd, None, Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout.len(), 1_000_000);
    }

    #[test]
    fn a_hung_command_is_killed_at_its_deadline() {
        let start = Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = output_within(&mut cmd, None, Duration::from_millis(200)).unwrap_err();
        assert!(format!("{err:#}").contains("did not finish"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
