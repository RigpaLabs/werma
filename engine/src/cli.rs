use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "werma", about = "Agent task orchestrator", version = crate::version_string())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Add a new task
    Add {
        /// Task prompt
        prompt: String,

        /// Output file path
        #[arg(short, long)]
        output: Option<String>,

        /// Priority (1-3, default 2)
        #[arg(short, long, default_value_t = 2)]
        priority: i32,

        /// Task type
        #[arg(short = 't', long = "type", default_value = "custom")]
        task_type: String,

        /// Model: opus|sonnet|haiku
        #[arg(short, long, default_value = "opus")]
        model: String,

        /// Override allowed tools (comma-separated)
        #[arg(long)]
        tools: Option<String>,

        /// Working directory (defaults to current directory)
        #[arg(short, long)]
        dir: Option<String>,

        /// Max turns (auto by type if not specified)
        #[arg(long)]
        turns: Option<i32>,

        /// Comma-separated task IDs to depend on
        #[arg(long)]
        depends: Option<String>,

        /// Comma-separated context files
        #[arg(long)]
        context: Option<String>,

        /// Linked Linear issue ID
        #[arg(long)]
        linear: Option<String>,

        /// Pipeline stage
        #[arg(long)]
        stage: Option<String>,

        /// Agent runtime: claude-code|codex|gemini-cli|qwen-code (default: claude-code)
        #[arg(long, default_value = "claude-code")]
        runtime: String,
    },

    /// List tasks (alias: ls)
    #[command(alias = "ls")]
    List {
        /// Filter by status (pending, running, completed, failed)
        status: Option<String>,
    },

    /// Run next pending task
    Run,

    /// Run all pending tasks (wave execution with dependency DAG)
    RunAll,

    /// Show task status summary
    #[command(alias = "st")]
    Status {
        /// Watch mode: auto-refresh
        #[arg(short, long)]
        watch: bool,

        /// Compact mode for narrow terminal panels
        #[arg(short, long)]
        compact: bool,

        /// Plain output: no colors, no art, tab-separated (for agents/piping)
        #[arg(short, long)]
        plain: bool,

        /// Refresh interval in seconds (default: 3, minimum: 1)
        #[arg(short, long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,

        /// Show all completed/failed/canceled tasks (default: last 17, configurable via config.toml)
        #[arg(short, long)]
        all: bool,
    },

    /// Show task details + output
    View {
        /// Task ID
        id: String,
    },

    /// Resume task with session_id
    #[command(alias = "cont")]
    Continue {
        /// Task ID
        id: String,
        /// Follow-up prompt
        prompt: Option<String>,
    },

    /// Reset failed task to pending
    Retry {
        /// Task ID
        id: String,
    },

    /// Kill tmux session and mark task as failed
    Kill {
        /// Task ID
        id: String,
    },

    /// Mark task as completed (called by exec script)
    Complete {
        /// Task ID
        id: String,

        /// Claude session ID
        #[arg(long)]
        session: Option<String>,

        /// Path to file containing result text
        #[arg(long)]
        result_file: Option<String>,

        /// Total cost in USD (parsed from Claude output JSON)
        #[arg(long)]
        cost: Option<f64>,

        /// Number of turns used by the agent
        #[arg(long)]
        turns: Option<i32>,
    },

    /// Mark task as failed (called by exec script)
    Fail {
        /// Task ID
        id: String,
    },

    /// Show live agent activity for a running task
    Peek {
        /// Task ID
        id: String,
    },

    /// Prune stale worktrees from completed/failed/canceled tasks (dry-run by default)
    Clean {
        /// Actually delete worktrees (default: dry-run, just list)
        #[arg(long)]
        force: bool,
    },

    /// Show logs
    Log {
        /// Task ID (if omitted, show most recent)
        id: Option<String>,
    },

    /// Daemon management
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonAction>,
    },

    /// Schedule management
    Sched {
        #[command(subcommand)]
        action: SchedAction,
    },

    /// Linear integration
    Linear {
        #[command(subcommand)]
        action: LinearAction,
    },

    /// Pipeline management
    Pipeline {
        #[command(subcommand)]
        action: PipelineAction,
    },

    /// Run code review on a PR, branch, or current changes
    Review {
        /// Target: PR URL, #number, or branch name
        target: Option<String>,

        /// Repository directory (defaults to current dir)
        #[arg(short, long)]
        dir: Option<String>,

        /// Force review even if one is already running for this target
        #[arg(short, long)]
        force: bool,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Dashboard (stub)
    Dash,

    /// Backup database (stub)
    Backup,

    /// Run database migrations (stub)
    Migrate,

    /// Build and upload macOS binary to GitHub Releases
    Build,

    /// Self-update from GitHub Releases
    Update,

    /// Show version info
    Version,

    /// Effects outbox management
    Effects {
        #[command(subcommand)]
        action: Option<EffectsAction>,
    },
}

#[derive(Debug, Subcommand)]
pub enum EffectsAction {
    /// List dead-lettered (permanently failed) effects
    Dead,

    /// Retry a dead or failed effect by ID
    Retry {
        /// Effect ID
        id: i64,
    },

    /// Show all effects for a given task
    History {
        /// Task ID
        task_id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Install daemon
    Install,
    /// Uninstall daemon
    Uninstall,
}

#[derive(Subcommand, Debug)]
pub enum SchedAction {
    /// Add a schedule
    Add {
        /// Schedule ID
        id: String,
        /// Cron expression (e.g. "30 7 * * *")
        cron: String,
        /// Prompt template
        prompt: String,

        /// Task type
        #[arg(short = 't', long = "type", default_value = "research")]
        task_type: String,

        /// Model
        #[arg(short, long, default_value = "opus")]
        model: String,

        /// Output file path
        #[arg(short, long)]
        output: Option<String>,

        /// Context files (comma-separated)
        #[arg(long)]
        context: Option<String>,

        /// Working directory (defaults to current directory)
        #[arg(short, long)]
        dir: Option<String>,

        /// Max turns
        #[arg(long)]
        turns: Option<i32>,
    },

    /// List schedules
    #[command(alias = "ls")]
    List,

    /// Remove a schedule
    Rm {
        /// Schedule ID
        id: String,
    },

    /// Enable a schedule
    On {
        /// Schedule ID
        id: String,
    },

    /// Disable a schedule
    Off {
        /// Schedule ID
        id: String,
    },

    /// Trigger a schedule manually
    Trigger {
        /// Schedule ID
        id: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum LinearAction {
    /// Setup Linear integration
    Setup,
    /// Sync issues from Linear
    Sync,
    /// Push task result to Linear
    Push {
        /// Task ID
        id: String,
    },
    /// Push all completed tasks to Linear
    PushAll,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Show current configuration (repo mappings, limits)
    Show,
}

#[derive(Subcommand, Debug)]
pub enum PipelineAction {
    /// Poll for new pipeline tasks
    Poll,
    /// Show pipeline status
    Status,
    /// Show pipeline config (stages, transitions)
    Show {
        /// Show only this stage
        #[arg(long)]
        stage: Option<String>,
        /// Pipeline name to show (default: current repo's pipeline)
        #[arg(long)]
        pipeline: Option<String>,
    },
    /// Validate pipeline YAML config
    Validate,
    /// Manually trigger a pipeline stage for Linear issues
    Run {
        /// Linear issue identifiers (e.g. RIG-95 RIG-100)
        issues: Vec<String>,

        /// Pipeline stage to run (default: analyst)
        #[arg(short, long)]
        stage: Option<String>,
    },
    /// Switch the active pipeline for a repo (edits ~/.werma/config.toml)
    Switch {
        /// Repo name (e.g. fathom, werma)
        repo: String,
        /// Pipeline name (e.g. default, economy)
        pipeline: String,
    },
}
