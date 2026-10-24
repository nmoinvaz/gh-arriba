use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "gh-arriba",
    bin_name = "gh arriba",
    about = "GitHub workflows that gh doesn't ship",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Target repository as OWNER/REPO (default: current repo)
    #[arg(short = 'R', long, global = true, value_name = "OWNER/REPO")]
    pub repo: Option<String>,

    /// Print machine-readable JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Skip confirmation prompts
    #[arg(short, long, global = true)]
    pub yes: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Review threads, pending reviews, retries, and rebases for pull requests
    #[command(subcommand)]
    Pr(PrCommand),
    /// Watch workflow runs and download their logs
    #[command(subcommand)]
    Run(RunCommand),
    /// Dispatch the repo's release workflow with a version bump
    Release(ReleaseArgs),
    /// Triage Dependabot pull requests and security alerts
    #[command(subcommand)]
    Dependabot(DependabotCommand),
    /// List self-hosted runners and what they are running
    Runners(RunnersArgs),
    /// Sync project config files to and from a gist
    Config(ConfigArgs),
}

// -------------------------------------------------------------------------
//  pr
// -------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum PrCommand {
    /// List unresolved review threads from humans and bots
    Threads(ThreadsArgs),
    /// Reply to a review comment or thread
    Reply(ReplyArgs),
    /// Resolve or unresolve review threads
    Resolve(ResolveArgs),
    /// Show or edit one comment by URL or ID
    #[command(subcommand)]
    Comment(CommentCommand),
    /// Manage your pending review and its inline comments
    #[command(subcommand)]
    Review(ReviewCommand),
    /// Apply reviewers' suggestion blocks to the working tree
    Apply(ApplyArgs),
    /// Write the whole discussion as grep-friendly text
    Dump(DumpArgs),
    /// Rerun failed and cancelled checks on a PR or all your PRs
    Retry(RetryArgs),
    /// Show failed checks with their failed steps and error lines
    Failures(FailuresArgs),
    /// Rebase a PR onto its base, signed locally or server-side
    Rebase(RebaseArgs),
    /// Suggest reviewers from top contributors and request one
    Reviewers(ReviewersArgs),
}

#[derive(Args)]
pub struct ThreadsArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only Copilot, CodeRabbit, and other bot threads
    #[arg(long, conflicts_with = "humans")]
    pub bots: bool,

    /// Only human threads
    #[arg(long)]
    pub humans: bool,

    /// Only threads started by this reviewer, @me for yours
    #[arg(long, value_name = "LOGIN")]
    pub author: Option<String>,

    /// Include resolved threads
    #[arg(long)]
    pub resolved: bool,

    /// Include outdated threads
    #[arg(long)]
    pub outdated: bool,

    /// Only threads opened or updated since your last review
    #[arg(long)]
    pub since_last_review: bool,

    /// Include CodeRabbit findings that live in review bodies
    #[arg(long)]
    pub review_bodies: bool,
}

#[derive(Args)]
pub struct ReplyArgs {
    /// Comment database ID, comment URL, or thread ID (PRRT_...)
    pub target: String,

    /// Reply text, or - to read stdin (default: stdin)
    pub body: Option<String>,

    /// Resolve the thread after replying
    #[arg(long)]
    pub resolve: bool,
}

#[derive(Args)]
pub struct ResolveArgs {
    /// Thread IDs (PRRT_...), comment database IDs, or comment URLs
    #[arg(required = true)]
    pub targets: Vec<String>,

    /// Unresolve instead
    #[arg(long)]
    pub undo: bool,
}

#[derive(Subcommand)]
pub enum CommentCommand {
    /// Print a review comment, issue comment, or review body
    Show(CommentShowArgs),
    /// Replace the body of a comment or review you posted
    Edit(CommentEditArgs),
}

#[derive(Args)]
pub struct CommentShowArgs {
    /// Comment URL (#discussion_r..., #issuecomment-..., #pullrequestreview-...) or ID
    #[arg(value_name = "URL|ID")]
    pub target: String,
}

#[derive(Args)]
pub struct CommentEditArgs {
    /// Comment URL (#discussion_r..., #issuecomment-..., #pullrequestreview-...) or ID
    #[arg(value_name = "URL|ID")]
    pub target: String,

    /// New text, or - to read stdin (default: stdin)
    pub body: Option<String>,
}

#[derive(Subcommand)]
pub enum ReviewCommand {
    /// Print the diff line ranges that accept inline comments
    Lines(PrArg),
    /// Add an inline comment to your pending review, creating it if needed
    Add(ReviewAddArgs),
    /// Print the comments in your pending review
    Show(PrArg),
    /// Submit your pending review as comment, approve, or request changes
    Submit(ReviewSubmitArgs),
    /// Delete your pending review and its comments
    Discard(PrArg),
}

#[derive(Args)]
pub struct PrArg {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,
}

#[derive(Args)]
pub struct ReviewAddArgs {
    /// File and line in the PR diff, as PATH:LINE or PATH:START-END
    #[arg(value_name = "PATH:LINE[-END]")]
    pub location: String,

    /// Comment text, or - to read stdin (default: stdin)
    pub body: Option<String>,

    /// Number, URL, or branch (default: PR for the current branch)
    #[arg(long)]
    pub pr: Option<String>,

    /// Diff side the line numbers refer to
    #[arg(long, value_enum, default_value_t = Side::Right)]
    pub side: Side,
}

#[derive(Args)]
pub struct ReviewSubmitArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Review verdict
    #[arg(long, value_enum, default_value_t = ReviewEvent::Comment)]
    pub event: ReviewEvent,

    /// Summary body for the review
    #[arg(long)]
    pub body: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Side {
    Left,
    Right,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ReviewEvent {
    Comment,
    Approve,
    RequestChanges,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only apply suggestions from these threads (PRRT_... or comment ID)
    #[arg(long = "thread", value_name = "ID")]
    pub threads: Vec<String>,

    /// Print the changes without writing files
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct DumpArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Write to this file instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Args)]
pub struct RetryArgs {
    /// Number, URL, or owner/repo#N, or owner/repo for all your PRs (default: current branch)
    pub target: Option<String>,

    /// Retry every open PR you authored in the repo
    #[arg(long)]
    pub mine: bool,

    /// Print the run tree without rerunning
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct FailuresArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Only checks whose name contains this text, repeatable
    #[arg(long = "job", value_name = "PATTERN")]
    pub jobs: Vec<String>,

    /// Directory to save full logs in (default: temp dir)
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Lines of log to show before each error
    #[arg(long, value_name = "N", default_value_t = crate::log::CONTEXT)]
    pub context: usize,

    /// Error blocks to show per job
    #[arg(long, value_name = "N", default_value_t = crate::log::MAX)]
    pub max: usize,
}

#[derive(Args)]
pub struct RebaseArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Rebase server-side, commits are unsigned
    #[arg(long, conflicts_with = "check")]
    pub remote: bool,

    /// Report conflicting files without rebasing
    #[arg(long)]
    pub check: bool,

    /// Don't comment "Rebased." on the PR
    #[arg(long)]
    pub no_comment: bool,
}

#[derive(Args)]
pub struct ReviewersArgs {
    /// Number, URL, or branch (default: PR for the current branch)
    pub pr: Option<String>,

    /// Request a review from this user, repeatable
    #[arg(long, value_name = "LOGIN")]
    pub add: Vec<String>,

    /// Number of contributors to suggest
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
}

// -------------------------------------------------------------------------
//  run
// -------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum RunCommand {
    /// Download job logs to files for searching
    Logs(LogsArgs),
    /// Follow runs until their jobs finish, failing if any job fails
    Watch(WatchArgs),
    /// Dispatch a workflow and print the run it starts
    Dispatch(DispatchArgs),
}

#[derive(Args)]
pub struct LogsArgs {
    /// Run ID, job ID, or Actions URL
    #[arg(value_name = "RUN_OR_JOB")]
    pub target: String,

    /// Only jobs that failed
    #[arg(long)]
    pub failed: bool,

    /// Download directory (default: temp dir)
    #[arg(short, long, value_name = "DIR")]
    pub out: Option<PathBuf>,

    /// Only jobs whose name contains this text, repeatable
    #[arg(long = "job", value_name = "PATTERN")]
    pub jobs: Vec<String>,

    /// Print each job's error lines after saving its log
    #[arg(long)]
    pub errors: bool,
}

#[derive(Args)]
pub struct WatchArgs {
    /// Run IDs, job IDs, or Actions URLs (default: every run for the local HEAD commit)
    #[arg(value_name = "RUN")]
    pub runs: Vec<String>,

    /// Watch every run for this commit
    #[arg(long, conflicts_with_all = ["runs", "branch"])]
    pub sha: Option<String>,

    /// Watch the runs for the newest commit with runs on this branch
    #[arg(long, conflicts_with = "runs")]
    pub branch: Option<String>,

    /// Only runs of this workflow, by name or file
    #[arg(short, long, value_name = "NAME", conflicts_with = "runs")]
    pub workflow: Option<String>,

    #[command(flatten)]
    pub wait: WaitArgs,
}

#[derive(Args)]
pub struct DispatchArgs {
    /// Workflow name, file name, or file name without extension
    pub workflow: String,

    /// Branch or tag to run the workflow on (default: the default branch)
    #[arg(long = "ref", value_name = "BRANCH")]
    pub git_ref: Option<String>,

    /// Set a workflow input, repeatable
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Follow the run until it finishes
    #[arg(long)]
    pub watch: bool,

    #[command(flatten)]
    pub wait: WaitArgs,
}

#[derive(Args)]
pub struct WaitArgs {
    /// Seconds between polls
    #[arg(long, value_name = "SECS", default_value_t = 20, value_parser = clap::value_parser!(u64).range(1..))]
    pub interval: u64,

    /// Minutes to wait before giving up
    #[arg(long, value_name = "MINS", default_value_t = 60)]
    pub timeout: u64,

    /// Only wait for jobs whose name contains this, ignoring case
    #[arg(long, value_name = "PATTERN")]
    pub job: Option<String>,
}

// -------------------------------------------------------------------------
//  release
// -------------------------------------------------------------------------

#[derive(Args)]
pub struct ReleaseArgs {
    /// Version part to increment
    #[arg(long, value_enum, default_value_t = Bump::Patch)]
    pub bump: Bump,

    /// Exact version, overrides --bump
    #[arg(long, value_name = "X.Y.Z")]
    pub version: Option<String>,

    /// Set a workflow input, repeatable
    #[arg(short = 'f', long = "field", value_name = "KEY=VALUE")]
    pub fields: Vec<String>,

    /// Workflow to dispatch (default: the one named release)
    #[arg(short, long, value_name = "NAME")]
    pub workflow: Option<String>,

    /// Branch to run the workflow on (default: the default branch)
    #[arg(long = "ref", value_name = "BRANCH")]
    pub git_ref: Option<String>,

    /// Follow the run until it finishes
    #[arg(long)]
    pub watch: bool,

    /// Print the plan without dispatching
    #[arg(long, conflicts_with = "watch")]
    pub dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Bump {
    Patch,
    Minor,
    Major,
}

// -------------------------------------------------------------------------
//  dependabot
// -------------------------------------------------------------------------

#[derive(Subcommand)]
pub enum DependabotCommand {
    /// Group open PRs into ready, needs rebase, and blocked, with lock-step sets
    List(DependabotListArgs),
    /// Approve and merge PRs, singly or as a lock-step group
    Merge(DependabotMergeArgs),
    /// Ask Dependabot to rebase PRs
    Rebase(DependabotRebaseArgs),
    /// List open security alerts with their fix PR
    Alerts(DependabotAlertsArgs),
    /// Dismiss alerts with a reason
    Dismiss(DependabotDismissArgs),
}

#[derive(Args)]
pub struct DependabotListArgs {
    /// Every repo in an organization
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Only PRs that fix a security alert
    #[arg(long)]
    pub security: bool,
}

#[derive(Args)]
pub struct DependabotMergeArgs {
    /// PRs to merge (default: prompt for each ready PR)
    pub prs: Vec<u64>,

    /// Merge every ready PR without prompting
    #[arg(long, conflicts_with = "prs")]
    pub all_ready: bool,

    /// Enable auto-merge, and request a rebase where a PR conflicts
    #[arg(long)]
    pub auto: bool,

    /// Comment posted before approving
    #[arg(long, value_name = "TEXT")]
    pub comment: Option<String>,

    /// Merge method
    #[arg(long, value_enum, default_value_t = MergeMethod::Rebase)]
    pub method: MergeMethod,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Args)]
pub struct DependabotRebaseArgs {
    /// PRs to rebase (default: prompt)
    pub prs: Vec<u64>,

    /// Rebase every open Dependabot PR
    #[arg(long, conflicts_with = "prs")]
    pub all: bool,
}

#[derive(Args)]
pub struct DependabotAlertsArgs {
    /// Every repo in an organization
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
}

#[derive(Args)]
pub struct DependabotDismissArgs {
    /// Alert numbers
    #[arg(required = true)]
    pub alerts: Vec<u64>,

    /// Why the alert is being dismissed
    #[arg(long, value_enum)]
    pub reason: DismissReason,

    /// Dismissal comment
    #[arg(long, value_name = "TEXT")]
    pub comment: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum DismissReason {
    #[value(name = "fix_started")]
    FixStarted,
    #[value(name = "inaccurate")]
    Inaccurate,
    #[value(name = "no_bandwidth")]
    NoBandwidth,
    #[value(name = "not_used")]
    NotUsed,
    #[value(name = "tolerable_risk")]
    TolerableRisk,
}

// -------------------------------------------------------------------------
//  runners
// -------------------------------------------------------------------------

#[derive(Args)]
pub struct RunnersArgs {
    /// Runners registered to an organization instead of the repo
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// List queued jobs even when no runner is busy, scanning every repo of an org
    #[arg(long)]
    pub queued: bool,

    /// Infer runners from recent jobs instead of the runners API, which needs admin access
    #[arg(long)]
    pub from_jobs: bool,

    /// Recent runs per repo to read when inferring runners from jobs
    #[arg(long, default_value_t = 30, value_name = "N")]
    pub runs: usize,

    /// Most recently pushed org repos to read when inferring runners from jobs
    #[arg(long, default_value_t = 10, value_name = "N")]
    pub repos: usize,
}

// -------------------------------------------------------------------------
//  config
// -------------------------------------------------------------------------

#[derive(Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: Option<ConfigCommand>,

    /// Gist name prefix (default: basename of project root)
    #[arg(long, global = true)]
    pub name: Option<String>,

    /// Root directory for relative paths (default: git root or cwd)
    #[arg(long, global = true)]
    pub root: Option<PathBuf>,

    /// Create the gist as secret instead of public
    #[arg(long, global = true)]
    pub private: bool,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Sync files with the gist (default when no command given)
    Sync {
        /// Files to sync, relative to root
        files: Vec<String>,
    },
    /// Report what sync would do without making any changes
    Status {
        /// Files to check, relative to root (default: all tracked files)
        files: Vec<String>,
    },
    /// Overwrite local files with the gist version, ignoring mtime
    Restore {
        /// Files to restore, relative to root (default: all tracked files)
        files: Vec<String>,
    },
    /// Add files to the tracked set and push them
    Add {
        /// Mark these files as specific to the current platform
        #[arg(long)]
        platform: bool,

        /// Files to add, relative to root
        files: Vec<String>,
    },
    /// Remove files from the tracked set and delete from gist
    Remove {
        /// Only remove the variant specific to the current platform
        #[arg(long)]
        platform: bool,

        /// Files to remove, relative to root
        files: Vec<String>,
    },
    /// Remove gist files not listed in the manifest
    Cleanup,
    /// Delete the entire config gist
    Delete,
    /// Open the project's gist in a web browser
    Open,
}
