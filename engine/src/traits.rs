use std::path::Path;

use anyhow::{Context, Result};

// ─── CommandOutput ───────────────────────────────────────────────────────────

/// Output from a command execution.
/// Custom struct because `std::process::ExitStatus` has no public constructor.
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl CommandOutput {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).trim().to_string()
    }

    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_string()
    }
}

// ─── CommandRunner trait ─────────────────────────────────────────────────────

/// Trait abstracting external command execution for testability.
/// All `Command::new("git"|"tmux"|"gh"|"osascript")` calls go through this.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput>;
}

/// Real implementation using std::process::Command.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput> {
        let mut cmd = std::process::Command::new(program);
        cmd.args(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let output = cmd.output().with_context(|| format!("running {program}"))?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

// ─── Notifier trait ──────────────────────────────────────────────────────────

/// Trait abstracting notifications (macOS + Slack) for testability.
/// Not yet consumed by daemon handlers — will be wired in a follow-up.
#[allow(dead_code)]
pub trait Notifier {
    fn notify_macos(&self, title: &str, message: &str, sound: &str);
    fn notify_slack(&self, channel: &str, text: &str);
}

/// Real notifier using osascript and Slack API.
pub struct RealNotifier;

impl Notifier for RealNotifier {
    fn notify_macos(&self, title: &str, message: &str, sound: &str) {
        crate::notify::notify_macos(title, message, sound);
    }

    fn notify_slack(&self, channel: &str, text: &str) {
        crate::notify::notify_slack(channel, text);
    }
}

// ─── Fakes (test-only) ──────────────────────────────────────────────────────

#[cfg(test)]
pub mod fakes {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Fake command runner with a FIFO queue of pre-programmed responses.
    /// Unmatched calls return success with empty output.
    pub struct FakeCommandRunner {
        responses: RefCell<VecDeque<CommandOutput>>,
        pub calls: RefCell<Vec<(String, Vec<String>, Option<String>)>>,
    }

    impl FakeCommandRunner {
        pub fn new() -> Self {
            Self {
                responses: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn push_success(&self, stdout: &str) {
            self.responses.borrow_mut().push_back(CommandOutput {
                success: true,
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            });
        }

        pub fn push_failure(&self, stderr: &str) {
            self.responses.borrow_mut().push_back(CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
            });
        }
    }

    impl CommandRunner for FakeCommandRunner {
        fn run(&self, program: &str, args: &[&str], dir: Option<&Path>) -> Result<CommandOutput> {
            self.calls.borrow_mut().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
                dir.map(|d| d.to_string_lossy().to_string()),
            ));

            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }))
        }
    }

    /// Fake notifier that records calls for assertion.
    pub struct FakeNotifier {
        pub macos_calls: RefCell<Vec<(String, String, String)>>,
        pub slack_calls: RefCell<Vec<(String, String)>>,
    }

    impl FakeNotifier {
        pub fn new() -> Self {
            Self {
                macos_calls: RefCell::new(Vec::new()),
                slack_calls: RefCell::new(Vec::new()),
            }
        }
    }

    impl Notifier for FakeNotifier {
        fn notify_macos(&self, title: &str, message: &str, sound: &str) {
            self.macos_calls.borrow_mut().push((
                title.to_string(),
                message.to_string(),
                sound.to_string(),
            ));
        }

        fn notify_slack(&self, channel: &str, text: &str) {
            self.slack_calls
                .borrow_mut()
                .push((channel.to_string(), text.to_string()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fakes::*;

    #[test]
    fn command_output_string_helpers() {
        let output = CommandOutput {
            success: true,
            stdout: b"hello world\n".to_vec(),
            stderr: b"warn\n".to_vec(),
        };
        assert_eq!(output.stdout_str(), "hello world");
        assert_eq!(output.stderr_str(), "warn");
    }

    #[test]
    fn real_command_runner_executes() {
        let cmd = RealCommandRunner;
        let output = cmd.run("echo", &["hello"], None).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout_str(), "hello");
    }

    #[test]
    fn fake_command_runner_fifo() {
        let cmd = FakeCommandRunner::new();
        cmd.push_success("output1");
        cmd.push_success("output2");
        cmd.push_failure("error");

        let r1 = cmd.run("git", &["status"], None).unwrap();
        assert!(r1.success);
        assert_eq!(r1.stdout_str(), "output1");

        let r2 = cmd.run("git", &["log"], None).unwrap();
        assert!(r2.success);
        assert_eq!(r2.stdout_str(), "output2");

        let r3 = cmd.run("gh", &["pr", "list"], None).unwrap();
        assert!(!r3.success);
        assert_eq!(r3.stderr_str(), "error");

        // Default: empty success
        let r4 = cmd.run("anything", &[], None).unwrap();
        assert!(r4.success);
        assert!(r4.stdout.is_empty());
    }

    #[test]
    fn fake_command_runner_records_calls() {
        let cmd = FakeCommandRunner::new();
        let dir = std::path::Path::new("/tmp");
        cmd.run("git", &["fetch"], Some(dir)).unwrap();

        let calls = cmd.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "git");
        assert_eq!(calls[0].1, vec!["fetch"]);
        assert_eq!(calls[0].2, Some("/tmp".to_string()));
    }

    #[test]
    fn fake_notifier_records_calls() {
        let n = FakeNotifier::new();
        n.notify_macos("title", "msg", "sound");
        n.notify_slack("#ch", "text");

        assert_eq!(n.macos_calls.borrow().len(), 1);
        assert_eq!(n.slack_calls.borrow().len(), 1);
        assert_eq!(n.macos_calls.borrow()[0].0, "title");
        assert_eq!(n.slack_calls.borrow()[0].1, "text");
    }
}
