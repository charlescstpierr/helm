//! Bounded, cancellable capture of external commands and their process groups.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
const TRUNCATED: &str = "\n[Sortie tronquée]\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }
}

pub async fn capture(
    program: &Path,
    args: &[String],
    cwd: &Path,
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<CommandOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Impossible de lancer {} : {error}", program.display()))?;
    let group = ProcessGroup(child.id().expect("spawned child has an id") as i32);
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut input = child.stdin.take();
    let mut out = OutputBuffer::default();
    let mut err = OutputBuffer::default();
    let completion = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            async {
                let status = child.wait().await?;
                // Shells can exit while background children still hold the pipes open.
                group.kill();
                Ok::<_, std::io::Error>(status)
            },
            out.read(&mut stdout),
            err.read(&mut stderr),
            async {
                if let (Some(pipe), Some(text)) = (input.as_mut(), stdin) {
                    pipe.write_all(text.as_bytes()).await?;
                    pipe.shutdown().await?;
                }
                input.take();
                Ok(())
            },
        )
    })
    .await;
    let (exit_code, timed_out) = match completion {
        Ok(Ok((status, (), (), ()))) => (status.code(), false),
        other => {
            group.kill();
            let _ = child.start_kill();
            let status = child.wait().await;
            if let Ok(Err(error)) = other {
                return Err(format!(
                    "Impossible de lire la commande {} : {error}",
                    program.display()
                ));
            }
            // Collect bytes already in the pipes after termination. The deadline also
            // covers descendants that deliberately detached from our process group.
            let _ = tokio::time::timeout(Duration::from_millis(100), async {
                tokio::try_join!(out.read(&mut stdout), err.read(&mut stderr))
            })
            .await;
            (status.ok().and_then(|status| status.code()), true)
        }
    };
    Ok(CommandOutput {
        stdout: out.finish(),
        stderr: err.finish(),
        exit_code,
        timed_out,
    })
}

struct ProcessGroup(i32);

impl ProcessGroup {
    fn kill(&self) {
        if self.0 > 1 {
            // SAFETY: this id belongs to the child spawned as a process-group leader.
            unsafe {
                libc::kill(-self.0, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Default)]
struct OutputBuffer {
    bytes: Vec<u8>,
    truncated: bool,
}

impl OutputBuffer {
    async fn read(&mut self, reader: &mut (impl AsyncRead + Unpin)) -> std::io::Result<()> {
        let mut chunk = [0_u8; 8192];
        loop {
            let count = reader.read(&mut chunk).await?;
            if count == 0 {
                return Ok(());
            }
            let keep = count.min(MAX_OUTPUT_BYTES - self.bytes.len());
            self.bytes.extend_from_slice(&chunk[..keep]);
            self.truncated |= keep < count;
        }
    }

    fn finish(self) -> String {
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        let truncated = self.truncated || text.len() > MAX_OUTPUT_BYTES;
        if truncated {
            let mut end = MAX_OUTPUT_BYTES.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(TRUNCATED);
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "helm-process-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn shell(command: &str, cwd: &Path, timeout: Duration) -> CommandOutput {
        capture(
            Path::new("sh"),
            &["-c".into(), command.into()],
            cwd,
            None,
            timeout,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn captures_stdin_stdout_stderr_and_exit_code() {
        let dir = TempDir::new();
        let output = capture(
            Path::new("sh"),
            &["-c".into(), "cat; printf problem >&2; exit 7".into()],
            &dir.0,
            Some("body\nwith $literal 'quotes'"),
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(output.stdout, "body\nwith $literal 'quotes'");
        assert_eq!(output.stderr, "problem");
        assert_eq!(output.exit_code, Some(7));
        assert!(!output.success());
        let output = shell("pwd", &dir.0, Duration::from_secs(2)).await;
        assert!(output.success());
        assert_eq!(output.stdout.trim(), dir.0.to_str().unwrap());
    }

    #[tokio::test]
    async fn drains_large_outputs_but_bounds_each_stream_separately() {
        let dir = TempDir::new();
        let output = shell(
            "head -c 1200000 /dev/zero; head -c 1300000 /dev/zero >&2",
            &dir.0,
            Duration::from_secs(3),
        )
        .await;
        assert!(output.success());
        for stream in [&output.stdout, &output.stderr] {
            assert_eq!(stream.len(), MAX_OUTPUT_BYTES + TRUNCATED.len());
            assert!(stream.ends_with(TRUNCATED));
        }
    }

    #[tokio::test]
    async fn timeout_preserves_output_and_kills_descendants() {
        let dir = TempDir::new();
        let output = shell(
            "printf before; (sleep 0.2; touch survived) & wait",
            &dir.0,
            Duration::from_millis(40),
        )
        .await;
        assert!(output.timed_out);
        assert!(!output.success());
        assert_eq!(output.stdout, "before");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!dir.0.join("survived").exists());
    }

    #[tokio::test]
    async fn normal_exit_kills_background_children_and_closes_their_pipes() {
        let dir = TempDir::new();
        let output = shell(
            "(sleep 0.2; touch survived) & printf done",
            &dir.0,
            Duration::from_secs(2),
        )
        .await;
        assert!(output.success());
        assert_eq!(output.stdout, "done");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!dir.0.join("survived").exists());
    }

    #[tokio::test]
    async fn cancelling_capture_kills_the_entire_process_group() {
        let dir = TempDir::new();
        let cwd = dir.0.clone();
        let task = tokio::spawn(async move {
            shell(
                "(sleep 0.2; touch survived) & touch ready; wait",
                &cwd,
                Duration::from_secs(5),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dir.0.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!dir.0.join("survived").exists());
    }

    #[tokio::test]
    async fn missing_executable_is_an_error() {
        let dir = TempDir::new();
        assert!(
            capture(
                Path::new("/no/such/helm-program"),
                &[],
                &dir.0,
                None,
                Duration::from_secs(1)
            )
            .await
            .is_err()
        );
    }
}
