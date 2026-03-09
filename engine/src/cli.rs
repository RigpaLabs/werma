use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "werma", about = "Agent task orchestrator", version = crate::version_string())]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
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

        /// Working directory
        #[arg(short, long, default_value = "~/projects/ar")]
        dir: String,

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

        /// Skip auto-creating Linear issue
        #[arg(long)]
        no_linear: bool,
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
    Status,

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

    /// Archive completed tasks
    Clean,

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
    },

    /// Dashboard (stub)
    Dash,

    /// Backup database (stub)
    Backup,

    /// Run database migrations (stub)
    Migrate,

    /// Show version info
    Version,
}

#[derive(Subcommand)]
pub enum DaemonAction {
    /// Install daemon
    Install,
    /// Uninstall daemon
    Uninstall,
}

#[derive(Subcommand)]
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

        /// Working directory
        #[arg(short, long, default_value = "~/projects/ar")]
        dir: String,

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

#[derive(Subcommand)]
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
    /// Mirror a task's state to Linear (create issue if needed, update status, post comment)
    Mirror {
        /// Task ID
        id: String,
    },
}

#[derive(Subcommand)]
pub enum PipelineAction {
    /// Poll for new pipeline tasks
    Poll,
    /// Show pipeline status
    Status,
    /// Handle pipeline callback for a completed task (db-driven)
    Callback {
        /// Task ID
        id: String,
    },
}
