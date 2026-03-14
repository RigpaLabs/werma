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

    /// Fake Linear API that records all calls and supports configurable failures.
    pub struct FakeLinearApi {
        pub move_calls: RefCell<Vec<(String, String)>>,
        pub comment_calls: RefCell<Vec<(String, String)>>,
        pub attach_calls: RefCell<Vec<(String, String, String)>>,
        pub estimate_calls: RefCell<Vec<(String, i32)>>,
        pub remove_label_calls: RefCell<Vec<(String, String)>>,
        pub add_label_calls: RefCell<Vec<(String, String)>>,
        pub issues_by_status: RefCell<std::collections::HashMap<String, Vec<serde_json::Value>>>,
        pub issues_by_label: RefCell<std::collections::HashMap<String, Vec<serde_json::Value>>>,
        pub issue_data: RefCell<std::collections::HashMap<String, (String, String)>>,
        /// Maps issue_id -> current status name (for get_issue_status reconciliation).
        pub issue_status: RefCell<std::collections::HashMap<String, String>>,
        fail_next_moves: RefCell<u32>,
    }

    #[allow(dead_code)]
    impl FakeLinearApi {
        pub fn new() -> Self {
            Self {
                move_calls: RefCell::new(Vec::new()),
                comment_calls: RefCell::new(Vec::new()),
                attach_calls: RefCell::new(Vec::new()),
                estimate_calls: RefCell::new(Vec::new()),
                remove_label_calls: RefCell::new(Vec::new()),
                add_label_calls: RefCell::new(Vec::new()),
                issues_by_status: RefCell::new(std::collections::HashMap::new()),
                issues_by_label: RefCell::new(std::collections::HashMap::new()),
                issue_data: RefCell::new(std::collections::HashMap::new()),
                issue_status: RefCell::new(std::collections::HashMap::new()),
                fail_next_moves: RefCell::new(0),
            }
        }

        /// Set the status that get_issue_status will return for an issue.
        pub fn set_issue_status(&self, issue_id: &str, status: &str) {
            self.issue_status
                .borrow_mut()
                .insert(issue_id.to_string(), status.to_string());
        }

        /// Configure the next N move_issue_by_name calls to return Err.
        pub fn fail_next_n_moves(&self, n: u32) {
            *self.fail_next_moves.borrow_mut() = n;
        }

        /// Add issues that will be returned by get_issues_by_status.
        pub fn set_issues_for_status(&self, status: &str, issues: Vec<serde_json::Value>) {
            self.issues_by_status
                .borrow_mut()
                .insert(status.to_string(), issues);
        }

        /// Add issues that will be returned by get_issues_by_label.
        pub fn set_issues_for_label(&self, label: &str, issues: Vec<serde_json::Value>) {
            self.issues_by_label
                .borrow_mut()
                .insert(label.to_string(), issues);
        }

        /// Set issue data returned by get_issue.
        pub fn set_issue_data(&self, id: &str, title: &str, description: &str) {
            self.issue_data
                .borrow_mut()
                .insert(id.to_string(), (title.to_string(), description.to_string()));
        }
    }

    impl crate::linear::LinearApi for FakeLinearApi {
        fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(self
                .issues_by_status
                .borrow()
                .get(status_name)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<serde_json::Value>> {
            Ok(self
                .issues_by_label
                .borrow()
                .get(label_name)
                .cloned()
                .unwrap_or_default())
        }

        fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
            let mut fail_count = self.fail_next_moves.borrow_mut();
            if *fail_count > 0 {
                *fail_count -= 1;
                return Err(anyhow::anyhow!(
                    "fake move failure: {} -> {}",
                    issue_id,
                    status_name
                ));
            }
            self.move_calls
                .borrow_mut()
                .push((issue_id.to_string(), status_name.to_string()));
            // Auto-update issue_status so reconciliation checks see the new status
            self.issue_status
                .borrow_mut()
                .insert(issue_id.to_string(), status_name.to_string());
            Ok(())
        }

        fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
            self.comment_calls
                .borrow_mut()
                .push((issue_id.to_string(), body.to_string()));
            Ok(())
        }

        fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
            self.attach_calls.borrow_mut().push((
                issue_id.to_string(),
                url.to_string(),
                title.to_string(),
            ));
            Ok(())
        }

        fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
            self.estimate_calls
                .borrow_mut()
                .push((issue_id.to_string(), estimate));
            Ok(())
        }

        fn get_issue(&self, issue_id: &str) -> Result<(String, String)> {
            Ok(self
                .issue_data
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default())
        }

        fn get_issue_by_identifier(
            &self,
            identifier: &str,
        ) -> Result<(String, String, String, String, Vec<String>)> {
            let (title, desc) = self
                .issue_data
                .borrow()
                .get(identifier)
                .cloned()
                .unwrap_or_default();
            Ok((
                format!("fake-uuid-{identifier}"),
                identifier.to_string(),
                title,
                desc,
                vec![],
            ))
        }

        fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
            self.remove_label_calls
                .borrow_mut()
                .push((issue_id.to_string(), label_name.to_string()));
            Ok(())
        }

        fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
            self.add_label_calls
                .borrow_mut()
                .push((issue_id.to_string(), label_name.to_string()));
            Ok(())
        }

        fn get_issue_status(&self, issue_id: &str) -> Result<String> {
            Ok(self
                .issue_status
                .borrow()
                .get(issue_id)
                .cloned()
                .unwrap_or_default())
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
