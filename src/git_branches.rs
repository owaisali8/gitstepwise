//! Branches through the `git` command line, the way GitHub Desktop offers
//! them: listing, creating and switching branches, merging one into the
//! current one, leaving changes on a branch (as a stash), and fetching,
//! pulling and pushing.
//!
//! Like the rest of `git.rs`, everything here blocks on `git`, so callers
//! run it on a background thread.

use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::time::SystemTime;

use crate::git::{Repo, git, run, run_text, web_url};

/// The message of a stash holding changes left on a branch. Git puts
/// "On <branch>: " before it.
const STASH_MESSAGE: &str = "Gitstepwise: changes left on ";
/// How many recently checked-out branches the list shows.
const RECENT: usize = 5;

/// A branch: one of the repository's own, or one only on a remote.
#[derive(Clone, Debug, PartialEq)]
pub struct Branch {
    /// Its name, without the remote's: "main", "feature/login".
    pub name: String,
    /// The remote it's on, for a branch that's only there.
    pub remote: Option<String>,
    /// When its last commit was made, in seconds since the Unix epoch.
    pub time: i64,
}

impl Branch {
    /// What Git calls it: "origin/feature" for a branch only on a remote.
    pub fn full_name(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// The repository's branches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Branches {
    /// The checked-out branch; `None` when none is (a detached HEAD).
    pub current: Option<String>,
    /// The default branch: the one `origin` points to, else `main` or
    /// `master`.
    pub default: Option<String>,
    /// Branches checked out lately, the latest first, leaving out the
    /// default branch.
    pub recent: Vec<String>,
    /// The repository's own branches, then those only on a remote, each
    /// by name.
    pub all: Vec<Branch>,
}

/// How the checked-out branch stands against its remote.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncStatus {
    /// The remote to fetch from: the upstream's, else `origin`.
    pub remote: String,
    /// The branch's upstream, like "origin/main"; `None` until it's
    /// published.
    pub upstream: Option<String>,
    /// Commits here that the upstream doesn't have, and the other way round.
    pub ahead: usize,
    pub behind: usize,
    /// When the repository last fetched.
    pub fetched: Option<SystemTime>,
}

/// What merging a branch into the current one would do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MergePreview {
    /// It has nothing the current branch doesn't.
    UpToDate,
    Clean { commits: usize },
    Conflicts { commits: usize, files: usize },
    /// This Git can't tell (it's older than 2.38).
    Unknown { commits: usize },
}

/// What a merge or a pull did.
#[derive(Clone, Debug, PartialEq)]
pub enum MergeOutcome {
    UpToDate,
    FastForward,
    Merged,
    /// It stopped on conflicts in these files, to be resolved and committed.
    Conflicts(Vec<String>),
}

impl Repo {
    /// Run a `git` command that changes something. On failure, the error is
    /// what Git said, tidied up.
    fn op(&self, args: &[&str]) -> Result<String, String> {
        let mut command = git(&self.root);
        command.args(args);
        op(&mut command)
    }

    fn remotes(&self) -> Vec<String> {
        run_text(git(&self.root).arg("remote"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The website of the `origin` remote, for GitHub and GitLab remotes.
    pub fn web_page(&self) -> Option<String> {
        let remote = run_text(git(&self.root).args(["remote", "get-url", "origin"]))?;
        web_url(remote.trim())
    }

    /// The checked-out branch; `None` when no branch is (a detached HEAD).
    pub fn current_branch(&self) -> Option<String> {
        run_text(git(&self.root).args(["symbolic-ref", "--short", "-q", "HEAD"]))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    pub fn branches(&self) -> Branches {
        let remotes = self.remotes();
        let Some(text) = run_text(git(&self.root).args([
            "for-each-ref",
            "--format=%(refname)%1f%(committerdate:unix)%1f%(upstream)%1f%(symref)",
            "refs/heads",
            "refs/remotes",
        ])) else {
            return Branches::default();
        };
        let mut local = Vec::new();
        let mut remote = Vec::new();
        // Remote branches a local branch tracks, which the list leaves out.
        let mut tracked = HashSet::new();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\x1f').collect();
            let [refname, time, upstream, symref] = fields[..] else {
                continue;
            };
            // `origin/HEAD` only points at another branch.
            if !symref.is_empty() {
                continue;
            }
            let time = time.parse().unwrap_or(0);
            if let Some(name) = refname.strip_prefix("refs/heads/") {
                if let Some(upstream) = upstream.strip_prefix("refs/remotes/") {
                    tracked.insert(upstream.to_string());
                }
                local.push(Branch {
                    name: name.to_string(),
                    remote: None,
                    time,
                });
            } else if let Some(rest) = refname.strip_prefix("refs/remotes/") {
                let Some(remote_name) = remotes
                    .iter()
                    .filter(|name| rest.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with('/')))
                    .max_by_key(|name| name.len())
                else {
                    continue;
                };
                remote.push(Branch {
                    name: rest[remote_name.len() + 1..].to_string(),
                    remote: Some(remote_name.clone()),
                    time,
                });
            }
        }
        let local_names: HashSet<String> = local.iter().map(|branch| branch.name.clone()).collect();
        remote.retain(|branch| !tracked.contains(&branch.full_name()) && !local_names.contains(&branch.name));
        // A branch on two remotes shows once.
        let mut seen = HashSet::new();
        remote.retain(|branch| seen.insert(branch.name.clone()));
        let by_name = |a: &Branch, b: &Branch| a.name.to_lowercase().cmp(&b.name.to_lowercase());
        local.sort_by(by_name);
        remote.sort_by(by_name);
        let mut all = local;
        all.extend(remote);

        let current = self.current_branch();
        let default = self.default_branch(&all);
        let recent = self.recent_branches(&local_names, current.as_deref(), default.as_deref());
        Branches {
            current,
            default,
            recent,
            all,
        }
    }

    fn default_branch(&self, all: &[Branch]) -> Option<String> {
        let from_origin = run_text(git(&self.root).args(["symbolic-ref", "--short", "-q", "refs/remotes/origin/HEAD"]))
            .and_then(|name| name.trim().strip_prefix("origin/").map(str::to_owned));
        let configured = run_text(git(&self.root).args(["config", "--get", "init.defaultBranch"]))
            .map(|name| name.trim().to_string());
        from_origin
            .into_iter()
            .chain(configured)
            .chain(["main".to_string(), "master".to_string()])
            .find(|name| all.iter().any(|branch| &branch.name == name))
    }

    /// From the checkouts in HEAD's reflog.
    fn recent_branches(&self, local: &HashSet<String>, current: Option<&str>, default: Option<&str>) -> Vec<String> {
        let mut recent: Vec<String> = current
            .filter(|current| Some(*current) != default)
            .map(str::to_owned)
            .into_iter()
            .collect();
        let text = run_text(git(&self.root).args(["log", "-g", "--format=%gs", "-n", "500", "HEAD"])).unwrap_or_default();
        for subject in text.lines() {
            if recent.len() >= RECENT {
                break;
            }
            let Some((_, to)) = subject
                .strip_prefix("checkout: moving from ")
                .and_then(|rest| rest.rsplit_once(" to "))
            else {
                continue;
            };
            if local.contains(to) && Some(to) != default && !recent.iter().any(|name| name == to) {
                recent.push(to.to_string());
            }
        }
        recent
    }

    pub fn sync_status(&self) -> Option<SyncStatus> {
        let remotes = self.remotes();
        if remotes.is_empty() {
            return None;
        }
        let upstream = run_text(git(&self.root).args(["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty());
        let remote = upstream
            .as_deref()
            .and_then(|upstream| {
                remotes
                    .iter()
                    .filter(|name| upstream.strip_prefix(name.as_str()).is_some_and(|rest| rest.starts_with('/')))
                    .max_by_key(|name| name.len())
            })
            .or_else(|| remotes.iter().find(|name| *name == "origin"))
            .unwrap_or(&remotes[0])
            .clone();
        let (ahead, behind) = upstream
            .as_ref()
            .and_then(|_| run_text(git(&self.root).args(["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])))
            .and_then(|text| {
                let (ahead, behind) = text.trim().split_once('\t')?;
                Some((ahead.parse().ok()?, behind.parse().ok()?))
            })
            .unwrap_or((0, 0));
        let fetched = std::fs::metadata(self.git_dir.join("FETCH_HEAD"))
            .and_then(|meta| meta.modified())
            .ok();
        Some(SyncStatus {
            remote,
            upstream,
            ahead,
            behind,
            fetched,
        })
    }

    /// Check out `branch`. A branch only on a remote gets a local branch
    /// that tracks it.
    pub fn switch_to(&self, branch: &Branch) -> Result<(), String> {
        match &branch.remote {
            None => self.op(&["switch", &branch.name]),
            Some(_) => self.op(&["switch", "--create", &branch.name, "--track", &branch.full_name()]),
        }
        .map(drop)
    }

    /// Make a branch named `name` from `base` (or else the current commit),
    /// and check it out. Changes not committed come along.
    pub fn create_branch(&self, name: &str, base: Option<&str>) -> Result<(), String> {
        let mut args = vec!["switch", "--create", name, "--no-track"];
        args.extend(base);
        self.op(&args).map(drop)
    }

    /// The name Git would give a branch called `name`, or why it can't.
    pub fn check_branch_name(&self, name: &str) -> Result<String, String> {
        run_text(git(&self.root).args(["check-ref-format", "--branch", name]))
            .map(|name| name.trim().to_string())
            .ok_or_else(|| format!("“{name}” can't be a branch name."))
    }

    /// Put the changes not committed aside, on `branch`, until it's checked
    /// out again.
    pub fn stash_changes(&self, branch: &str) -> Result<(), String> {
        let message = format!("{STASH_MESSAGE}{branch}");
        self.op(&["stash", "push", "--include-untracked", "--message", &message])
            .map(drop)
    }

    /// The stash of changes left on `branch`, like "stash@{0}", if there's one.
    pub fn stash_for(&self, branch: &str) -> Option<String> {
        let text = run_text(git(&self.root).args(["stash", "list", "--format=%gd%x1f%gs"]))?;
        let suffix = format!(": {STASH_MESSAGE}{branch}");
        text.lines().find_map(|line| {
            let (id, subject) = line.split_once('\x1f')?;
            subject.ends_with(&suffix).then(|| id.to_string())
        })
    }

    pub fn restore_stash(&self, stash: &str) -> Result<(), String> {
        self.op(&["stash", "pop", stash]).map(drop)
    }

    pub fn drop_stash(&self, stash: &str) -> Result<(), String> {
        self.op(&["stash", "drop", stash]).map(drop)
    }

    /// Whether a merge stopped on conflicts and is waiting for a commit.
    pub fn is_merging(&self) -> bool {
        self.git_dir.join("MERGE_HEAD").exists()
    }

    /// Files with conflicts left to resolve.
    pub fn conflicted_paths(&self) -> Vec<String> {
        run(git(&self.root).args(["diff", "--name-only", "--diff-filter=U", "-z"]))
            .map(|output| {
                output
                    .split(|byte| *byte == 0)
                    .filter_map(|path| std::str::from_utf8(path).ok())
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What merging `from` into the current branch would do, without doing it.
    pub fn merge_preview(&self, from: &str) -> MergePreview {
        let commits = run_text(git(&self.root).args(["rev-list", "--count"]).arg(format!("HEAD..{from}")))
            .and_then(|count| count.trim().parse().ok())
            .unwrap_or(0);
        if commits == 0 {
            return MergePreview::UpToDate;
        }
        let output = git(&self.root)
            .args(["merge-tree", "--write-tree", "--name-only", "--no-messages", "HEAD", from])
            .stdin(Stdio::null())
            .output();
        match output {
            Ok(output) if output.status.code() == Some(0) => MergePreview::Clean { commits },
            Ok(output) if output.status.code() == Some(1) => {
                // The merged tree's id, then a conflicted file on each line.
                let files = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .skip(1)
                    .filter(|line| !line.is_empty())
                    .count();
                MergePreview::Conflicts { commits, files }
            }
            _ => MergePreview::Unknown { commits },
        }
    }

    /// Merge `from` into the current branch.
    pub fn merge(&self, from: &str) -> Result<MergeOutcome, String> {
        let before = self.head();
        let target = run_text(git(&self.root).args(["rev-parse", "-q", "--verify"]).arg(format!("{from}^{{commit}}")))
            .map(|id| id.trim().to_string());
        let result = self.op(&["merge", "--no-edit", from]);
        self.merge_outcome(result, before, target)
    }

    pub fn abort_merge(&self) -> Result<(), String> {
        self.op(&["merge", "--abort"]).map(drop)
    }

    pub fn fetch(&self, remote: &str) -> Result<(), String> {
        self.op(&["fetch", "--prune", remote]).map(drop)
    }

    /// Fetch and merge the upstream's commits. Without a setting for how,
    /// they're merged, as GitHub Desktop does.
    pub fn pull(&self) -> Result<MergeOutcome, String> {
        let configured = |key: &str| run_text(git(&self.root).args(["config", "--get", key])).is_some();
        let mut args = vec!["pull", "--no-edit"];
        if !configured("pull.rebase") && !configured("pull.ff") {
            args.push("--no-rebase");
        }
        let before = self.head();
        let target = run_text(git(&self.root).args(["rev-parse", "-q", "--verify", "@{upstream}"]))
            .map(|id| id.trim().to_string());
        let result = self.op(&args);
        // The upstream moves with the fetch.
        let target = run_text(git(&self.root).args(["rev-parse", "-q", "--verify", "@{upstream}"]))
            .map(|id| id.trim().to_string())
            .or(target);
        self.merge_outcome(result, before, target)
    }

    fn merge_outcome(
        &self,
        result: Result<String, String>,
        before: Option<String>,
        target: Option<String>,
    ) -> Result<MergeOutcome, String> {
        if let Err(err) = result {
            if self.is_merging() {
                return Ok(MergeOutcome::Conflicts(self.conflicted_paths()));
            }
            return Err(err);
        }
        let after = self.head();
        Ok(if after == before {
            MergeOutcome::UpToDate
        } else if after.is_some() && after == target {
            MergeOutcome::FastForward
        } else {
            MergeOutcome::Merged
        })
    }

    /// Push the current branch to its upstream; or, when it has none yet,
    /// publish it to `remote` and track it there.
    pub fn push(&self, status: &SyncStatus, branch: &str) -> Result<(), String> {
        match &status.upstream {
            Some(upstream) => {
                let name = upstream
                    .strip_prefix(&format!("{}/", status.remote))
                    .unwrap_or(branch);
                self.op(&["push", &status.remote, &format!("HEAD:refs/heads/{name}")])
            }
            None => self.op(&["push", "--set-upstream", &status.remote, branch]),
        }
        .map(drop)
    }
}

/// Run a `git` command that changes something, never waiting on a prompt
/// for a password: without one from a credential helper, it fails.
fn op(command: &mut Command) -> Result<String, String> {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .map_err(|err| format!("Couldn't run git: {err}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let said = error_text(&output.stderr, &output.stdout);
    Err(if said.is_empty() {
        "Git stopped with an error.".to_string()
    } else {
        said
    })
}

/// What Git said when it failed, without its hints and "error:" labels.
fn error_text(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stdout = String::from_utf8_lossy(stdout);
    let text = if stderr.trim().is_empty() { stdout } else { stderr };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("hint:"))
        .map(|line| {
            line.strip_prefix("error: ")
                .or_else(|| line.strip_prefix("fatal: "))
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A name Git accepts for a branch, from what was typed: spaces and
/// characters Git refuses become hyphens, as GitHub Desktop does it.
pub fn sanitize_branch_name(typed: &str) -> String {
    let mut name = String::new();
    for c in typed.trim().chars() {
        let bad = c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\');
        let c = if bad { '-' } else { c };
        // No "--", "..", "//" or "@{".
        if (c == '-' || c == '.' || c == '/') && name.ends_with(c) {
            continue;
        }
        if c == '{' && name.ends_with('@') {
            name.pop();
            name.push('-');
            continue;
        }
        name.push(c);
    }
    let name = name.trim_start_matches(['-', '.', '/']);
    let mut name = name.trim_end_matches(['.', '/']).to_string();
    while name.ends_with(".lock") {
        name.truncate(name.len() - ".lock".len());
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Run git in `dir` as a test user, and return what it printed.
    fn sh(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gitstepwise-branches-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn commit(dir: &Path, file: &str, text: &str, message: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-q", "-m", message]);
    }

    /// A repository with one commit on `main`, as the test user.
    fn repository(name: &str) -> (PathBuf, Repo) {
        let dir = folder(name);
        sh(&dir, &["init", "-q", "-b", "main"]);
        // For the commits the app makes itself, like merges.
        sh(&dir, &["config", "user.name", "Test"]);
        sh(&dir, &["config", "user.email", "test@example.com"]);
        sh(&dir, &["config", "commit.gpgsign", "false"]);
        commit(&dir, "a.txt", "one\n", "First");
        let repo = Repo::find(&dir).unwrap();
        (dir, repo)
    }

    #[test]
    fn branches_are_listed_with_the_default_and_recent_ones() {
        let (dir, repo) = repository("list");
        sh(&dir, &["branch", "zeta"]);
        sh(&dir, &["branch", "Alpha"]);
        sh(&dir, &["switch", "-q", "zeta"]);
        sh(&dir, &["switch", "-q", "Alpha"]);
        let branches = repo.branches();
        assert_eq!(branches.current.as_deref(), Some("Alpha"));
        assert_eq!(branches.default.as_deref(), Some("main"));
        assert_eq!(branches.recent, ["Alpha", "zeta"]);
        let names: Vec<&str> = branches.all.iter().map(|branch| branch.name.as_str()).collect();
        assert_eq!(names, ["Alpha", "main", "zeta"]);
        assert!(branches.all[0].time > 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn creating_and_switching_branches() {
        let (dir, repo) = repository("switch");
        assert_eq!(repo.check_branch_name("feature/login").unwrap(), "feature/login");
        assert!(repo.check_branch_name("bad name").is_err());
        repo.create_branch("feature", None).unwrap();
        assert_eq!(repo.branch().as_deref(), Some("feature"));
        let err = repo.create_branch("feature", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        let main = Branch {
            name: "main".into(),
            remote: None,
            time: 0,
        };
        repo.switch_to(&main).unwrap();
        assert_eq!(repo.branch().as_deref(), Some("main"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn changes_left_on_a_branch_come_back() {
        let (dir, repo) = repository("stash");
        sh(&dir, &["branch", "other"]);
        std::fs::write(dir.join("a.txt"), "edited\n").unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        repo.stash_changes("main").unwrap();
        assert!(repo.changed_paths().is_empty());
        assert!(repo.stash_for("other").is_none());
        let stash = repo.stash_for("main").unwrap();
        repo.restore_stash(&stash).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "edited\n");
        assert!(dir.join("new.txt").exists());
        assert!(repo.stash_for("main").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn merging_a_branch_cleanly_and_with_conflicts() {
        let (dir, repo) = repository("merge");
        sh(&dir, &["switch", "-q", "-c", "clean"]);
        commit(&dir, "b.txt", "b\n", "Add b");
        sh(&dir, &["switch", "-q", "-c", "clash", "main"]);
        commit(&dir, "a.txt", "clash\n", "Change a one way");
        sh(&dir, &["switch", "-q", "main"]);
        commit(&dir, "a.txt", "main\n", "Change a the other way");

        assert_eq!(repo.merge_preview("main"), MergePreview::UpToDate);
        assert!(matches!(
            repo.merge_preview("clean"),
            MergePreview::Clean { commits: 1 } | MergePreview::Unknown { commits: 1 }
        ));
        assert!(matches!(
            repo.merge_preview("clash"),
            MergePreview::Conflicts { commits: 1, files: 1 } | MergePreview::Unknown { commits: 1 }
        ));

        assert_eq!(repo.merge("clean").unwrap(), MergeOutcome::Merged);
        assert_eq!(repo.merge("clean").unwrap(), MergeOutcome::UpToDate);
        assert_eq!(repo.merge("clash").unwrap(), MergeOutcome::Conflicts(vec!["a.txt".into()]));
        assert!(repo.is_merging());
        repo.abort_merge().unwrap();
        assert!(!repo.is_merging());
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "main\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn fetching_pulling_pushing_and_publishing() {
        let (upstream_dir, _) = repository("remote-source");
        let bare = folder("remote-bare");
        sh(&bare, &["clone", "-q", "--bare", upstream_dir.to_str().unwrap(), "."]);
        let mine = folder("remote-mine");
        sh(&mine, &["clone", "-q", bare.to_str().unwrap(), "."]);
        sh(&mine, &["config", "user.name", "Test"]);
        sh(&mine, &["config", "user.email", "test@example.com"]);
        sh(&mine, &["config", "commit.gpgsign", "false"]);
        let theirs = folder("remote-theirs");
        sh(&theirs, &["clone", "-q", bare.to_str().unwrap(), "."]);
        let repo = Repo::find(&mine).unwrap();

        // Someone else pushes a commit and a branch.
        commit(&theirs, "theirs.txt", "x\n", "Theirs");
        sh(&theirs, &["push", "-q", "origin", "main"]);
        sh(&theirs, &["push", "-q", "origin", "main:shared"]);

        let status = repo.sync_status().unwrap();
        assert_eq!(status.remote, "origin");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (0, 0));
        repo.fetch("origin").unwrap();
        let status = repo.sync_status().unwrap();
        assert_eq!((status.ahead, status.behind), (0, 1));
        assert!(status.fetched.is_some());

        // The branch only on the remote is listed, and can be checked out.
        let branches = repo.branches();
        assert_eq!(branches.default.as_deref(), Some("main"));
        let shared = branches.all.iter().find(|branch| branch.name == "shared").unwrap();
        assert_eq!(shared.remote.as_deref(), Some("origin"));
        assert!(!branches.all.iter().any(|branch| branch.full_name() == "origin/main"));

        assert_eq!(repo.pull().unwrap(), MergeOutcome::FastForward);
        assert!(mine.join("theirs.txt").exists());

        commit(&mine, "mine.txt", "y\n", "Mine");
        assert_eq!(repo.sync_status().unwrap().ahead, 1);
        repo.push(&repo.sync_status().unwrap(), "main").unwrap();
        assert_eq!(repo.sync_status().unwrap().ahead, 0);

        // A new branch is published, then tracks its remote branch.
        repo.create_branch("topic", None).unwrap();
        let status = repo.sync_status().unwrap();
        assert_eq!(status.upstream, None);
        repo.push(&status, "topic").unwrap();
        assert_eq!(repo.sync_status().unwrap().upstream.as_deref(), Some("origin/topic"));

        repo.switch_to(shared).unwrap();
        assert_eq!(repo.branch().as_deref(), Some("shared"));
        assert_eq!(repo.sync_status().unwrap().upstream.as_deref(), Some("origin/shared"));

        for dir in [upstream_dir, bare, mine, theirs] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn typed_names_become_branch_names() {
        assert_eq!(sanitize_branch_name("  fix the login bug "), "fix-the-login-bug");
        assert_eq!(sanitize_branch_name("feature//a..b"), "feature/a.b");
        assert_eq!(sanitize_branch_name("what?*[x]"), "what-x]");
        assert_eq!(sanitize_branch_name("-.start"), "start");
        assert_eq!(sanitize_branch_name("end.lock"), "end");
        assert_eq!(sanitize_branch_name("a@{b"), "a-b");
        assert_eq!(sanitize_branch_name("feature/login"), "feature/login");
    }

    #[test]
    fn git_errors_lose_their_labels_and_hints() {
        let said = error_text(b"error: pathspec 'x' did not match\nhint: try this\n", b"");
        assert_eq!(said, "pathspec 'x' did not match");
        assert_eq!(error_text(b"", b"fatal: no remote\n"), "no remote");
    }
}
