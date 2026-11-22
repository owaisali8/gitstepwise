//! Git integration through the `git` command line: the committed version of
//! a file, the repository's changed files, and line-level diffs between the
//! committed text and the text being edited.
//!
//! Everything here blocks on `git`, so callers run it on a background thread.

use std::collections::HashSet;
use std::io::Write as _;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use similar::{Algorithm, DiffOp, capture_diff_slices_deadline};

/// Give up on an exact diff after this long and accept a coarser one.
const DIFF_DEADLINE: Duration = Duration::from_millis(500);

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(dir);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Don't flash a console window when started from a GUI.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Run `git` and return what it printed, or `None` if it failed or isn't
/// installed.
fn run(command: &mut Command) -> Option<Vec<u8>> {
    let output = command.output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn run_text(command: &mut Command) -> Option<String> {
    String::from_utf8(run(command)?).ok()
}

/// The repository a folder belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Repo {
    /// The top of the working tree.
    pub root: PathBuf,
    /// The `.git` directory, whose files change on commits, checkouts and staging.
    pub git_dir: PathBuf,
}

impl Repo {
    pub fn find(dir: &Path) -> Option<Self> {
        let text = run_text(git(dir).args(["rev-parse", "--show-toplevel", "--absolute-git-dir"]))?;
        let mut lines = text.lines();
        let root = PathBuf::from(lines.next()?);
        let git_dir = PathBuf::from(lines.next()?);
        Some(Self {
            root: std::fs::canonicalize(&root).map_or(root, strip_verbatim_prefix),
            git_dir,
        })
    }

    /// The checked-out branch, or the short commit id when detached.
    pub fn branch(&self) -> Option<String> {
        run_text(git(&self.root).args(["symbolic-ref", "--short", "-q", "HEAD"]))
            .or_else(|| run_text(git(&self.root).args(["rev-parse", "--short", "HEAD"])))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
    }

    /// Changes when something is committed, checked out or staged, so the
    /// committed versions of files need reading again.
    pub fn stamp(&self) -> Vec<Option<SystemTime>> {
        ["HEAD", "index", "logs/HEAD"]
            .iter()
            .map(|file| std::fs::metadata(self.git_dir.join(file)).and_then(|m| m.modified()).ok())
            .collect()
    }

    /// Paths, relative to the root and with `/` separators, of files that differ
    /// from the last commit: modified, added, deleted and untracked ones.
    pub fn changed_paths(&self) -> Vec<String> {
        let Some(output) = run(git(&self.root).args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ])) else {
            return Vec::new();
        };
        // Each entry is "XY path"; without renames there are no second paths.
        let mut paths: Vec<String> = output
            .split(|byte| *byte == 0)
            .filter_map(|entry| std::str::from_utf8(entry).ok()?.get(3..).map(str::to_owned))
            .filter(|path| !path.is_empty() && !path.ends_with('/'))
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// The commit checked out, or `None` before the first commit.
    pub fn head(&self) -> Option<String> {
        run_text(git(&self.root).args(["rev-parse", "-q", "--verify", "HEAD"]))
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    }

    /// Which of `paths` (relative, `/`-separated) Git ignores, by the
    /// repository's `.gitignore` files and excludes. Tracked files never are.
    pub fn ignored(&self, paths: &[String]) -> HashSet<String> {
        if paths.is_empty() {
            return HashSet::new();
        }
        let mut command = git(&self.root);
        command
            .args(["check-ignore", "--stdin", "-z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = command.spawn() else {
            return HashSet::new();
        };
        if let Some(mut stdin) = child.stdin.take() {
            let input: Vec<u8> = paths.iter().flat_map(|path| path.bytes().chain([0])).collect();
            let _ = stdin.write_all(&input);
        }
        // Exits with 1 when nothing is ignored, so read the output regardless.
        let Ok(output) = child.wait_with_output() else {
            return HashSet::new();
        };
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter_map(|path| std::str::from_utf8(path).ok())
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// A file's contents in the last commit, by its path relative to the root,
    /// or `None` if it isn't in the last commit.
    pub fn committed_bytes(&self, relative: &str) -> Option<Vec<u8>> {
        // `cat-file blob` refuses folders, where `show` would list them.
        run(git(&self.root).args(["cat-file", "blob"]).arg(format!("HEAD:{relative}")))
    }

    /// `path` relative to the root, with `/` separators.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let path = std::fs::canonicalize(path).map_or_else(|_| path.to_path_buf(), strip_verbatim_prefix);
        let relative = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<_> = relative.iter().map(|part| part.to_string_lossy()).collect();
        Some(parts.join("/"))
    }

    /// The latest `limit` commits on the checked-out branch, newest first;
    /// only those that touch `path` (a file or folder, relative) if given.
    pub fn log(&self, path: Option<&str>, limit: usize) -> Vec<Commit> {
        let mut command = git(&self.root);
        // Fields end in 0x1f and commits in 0x1e, which messages never contain.
        command
            .args(["log", "--format=%H%x1f%h%x1f%an%x1f%at%x1f%s%x1e"])
            .arg(format!("-n{limit}"));
        if let Some(path) = path {
            command.arg("--").arg(path);
        }
        let Some(text) = run_text(&mut command) else {
            return Vec::new();
        };
        text.split('\x1e')
            .filter_map(|record| {
                let mut fields = record.trim_start_matches(['\n', '\r']).split('\x1f');
                Some(Commit {
                    id: fields.next()?.to_string(),
                    short_id: fields.next()?.to_string(),
                    author: fields.next()?.to_string(),
                    time: fields.next()?.trim().parse().ok()?,
                    subject: fields.next()?.to_string(),
                })
            })
            .collect()
    }

    /// One patch per file `commit` changed, against its first parent (or
    /// against nothing, for the first commit); only for `path` if given.
    /// At most `limit` files; the second value says whether there were more.
    pub fn commit_patches(&self, commit: &str, path: Option<&str>, limit: usize) -> (Vec<String>, bool) {
        let parent = run_text(git(&self.root).args(["rev-parse", "-q", "--verify"]).arg(format!("{commit}^1")))
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty());
        let mut command = git(&self.root);
        match &parent {
            Some(parent) => command.args(["diff", "--name-only", "-z", "--no-renames", parent.as_str(), commit]),
            None => command.args([
                "diff-tree",
                "-r",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-z",
                "--no-renames",
                commit,
            ]),
        };
        if let Some(path) = path {
            command.arg("--").arg(path);
        }
        let Some(output) = run(&mut command) else {
            return (Vec::new(), false);
        };
        let paths: Vec<&str> = output
            .split(|byte| *byte == 0)
            .filter_map(|path| std::str::from_utf8(path).ok())
            .filter(|path| !path.is_empty())
            .collect();
        let patches = paths
            .iter()
            .take(limit)
            .filter_map(|path| {
                let old = parent.as_deref().and_then(|parent| self.blob(parent, path));
                file_patch(path, old, self.blob(commit, path))
            })
            .collect();
        (patches, paths.len() > limit)
    }

    /// A file's contents at a commit, or `None` if it isn't there.
    fn blob(&self, commit: &str, relative: &str) -> Option<Vec<u8>> {
        run(git(&self.root).args(["cat-file", "blob"]).arg(format!("{commit}:{relative}")))
    }

    /// A link to `relative` at commit `commit` on the `origin` remote's
    /// website, for GitHub and GitLab remotes.
    pub fn permalink(&self, commit: &str, relative: &str) -> Result<String, String> {
        let remote = run_text(git(&self.root).args(["remote", "get-url", "origin"]))
            .map(|url| url.trim().to_string())
            .ok_or("The repository has no \"origin\" remote.")?;
        let site = web_url(&remote).ok_or_else(|| format!("Links aren't supported for the remote {remote}."))?;
        let path = relative.split('/').map(percent_encode).collect::<Vec<_>>().join("/");
        if site.starts_with("https://github.com/") {
            Ok(format!("{site}/blob/{commit}/{path}"))
        } else if site.starts_with("https://gitlab.com/") {
            Ok(format!("{site}/-/blob/{commit}/{path}"))
        } else {
            Err(format!("Links are only made for GitHub and GitLab remotes, not {site}."))
        }
    }
}

/// One commit in the history.
#[derive(Clone, Debug)]
pub struct Commit {
    pub id: String,
    pub short_id: String,
    pub author: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
    pub subject: String,
}

/// The website of a remote: `git@github.com:owner/repo.git`,
/// `ssh://git@github.com/owner/repo` and `https://github.com/owner/repo.git`
/// all give `https://github.com/owner/repo`.
fn web_url(remote: &str) -> Option<String> {
    let rest = if let Some(rest) = remote.strip_prefix("https://").or_else(|| remote.strip_prefix("http://")) {
        rest.to_string()
    } else if let Some(rest) = remote.strip_prefix("ssh://") {
        rest.to_string()
    } else {
        // scp-like: user@host:owner/repo
        let (host, path) = remote.split_once(':')?;
        format!("{host}/{path}")
    };
    // Drop any user name, then any port.
    let rest = rest.rsplit_once('@').map_or(rest.as_str(), |(_, rest)| rest);
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next()?;
    let path = path.trim_end_matches('/').trim_end_matches(".git");
    // A local path like C:/repos/x isn't a website.
    (host.contains('.') && !path.starts_with('/') && path.contains('/')).then(|| format!("https://{host}/{path}"))
}

/// Percent-encode one path segment for a URL.
fn percent_encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// The text of `file` in the last commit, or `None` when it isn't committed
/// (untracked, new, or not in a repository).
pub fn committed_text(file: &Path) -> Option<String> {
    let dir = file.parent()?;
    let name = file.file_name()?.to_str()?;
    run_text(git(dir).arg("show").arg(format!("HEAD:./{name}")))
}

/// `canonicalize` on Windows returns `\\?\C:\...`; drop the prefix so paths
/// compare equal to the ones Git and the rest of the app use.
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(stripped) if !stripped.starts_with("UNC\\") => PathBuf::from(stripped),
        _ => path,
    }
}

/// Lines without their endings, so a CRLF working copy compares equal to the
/// LF text Git stores. A final line ending doesn't start another line.
fn lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn diff(old: &[&str], new: &[&str]) -> Vec<DiffOp> {
    capture_diff_slices_deadline(Algorithm::Myers, old, new, Some(Instant::now() + DIFF_DEADLINE))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MarkerKind {
    Added,
    Modified,
    Deleted,
}

/// A changed stretch of the edited text, for the editor's gutter.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub kind: MarkerKind,
    /// Byte range of the changed lines in the edited text. For deletions it's
    /// empty: the line boundary where lines were removed.
    pub range: Range<usize>,
}

/// Where `text` differs from the committed `base`, line by line.
pub fn markers(base: &str, text: &str) -> Vec<Marker> {
    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(ix, _)| ix + 1))
        .collect();
    let offset = |line: usize| line_starts.get(line).copied().unwrap_or(text.len()).min(text.len());
    let span = |start: usize, len: usize| offset(start)..offset(start + len);

    diff(&lines(base), &lines(text))
        .into_iter()
        .filter_map(|op| {
            let (kind, range) = match op {
                DiffOp::Equal { .. } => return None,
                DiffOp::Insert { new_index, new_len, .. } => (MarkerKind::Added, span(new_index, new_len)),
                DiffOp::Replace { new_index, new_len, .. } => (MarkerKind::Modified, span(new_index, new_len)),
                DiffOp::Delete { new_index, .. } => (MarkerKind::Deleted, offset(new_index)..offset(new_index)),
            };
            Some(Marker { kind, range })
        })
        .collect()
}

/// Like [`patch`], from raw contents: a file that isn't UTF-8 text gets
/// Git's "Binary files differ" notice instead of lines.
pub fn file_patch(path: &str, old: Option<Vec<u8>>, new: Option<Vec<u8>>) -> Option<String> {
    if old.is_some() == new.is_some() && old == new {
        return None;
    }
    fn text(bytes: &Option<Vec<u8>>) -> Result<Option<&str>, std::str::Utf8Error> {
        bytes.as_deref().map(std::str::from_utf8).transpose()
    }
    match (text(&old), text(&new)) {
        (Ok(old), Ok(new)) => patch(path, old, new),
        _ => Some(binary_patch(path, old.is_some(), new.is_some())),
    }
}

/// Git's notice for a changed file that isn't text.
pub fn binary_patch(path: &str, old_exists: bool, new_exists: bool) -> String {
    let side = |exists: bool, prefix: &str| {
        if exists {
            format!("{prefix}/{path}")
        } else {
            "/dev/null".to_string()
        }
    };
    format!(
        "diff --git a/{path} b/{path}\nBinary files {} and {} differ\n",
        side(old_exists, "a"),
        side(new_exists, "b"),
    )
}

/// Whether two texts have the same lines, ignoring CRLF versus LF.
pub fn same_lines(a: &str, b: &str) -> bool {
    lines(a) == lines(b)
}

/// A Git-style patch for one file, or `None` if nothing changed. `None` for a
/// side means the file doesn't exist there. Every unchanged line is included
/// as context so the viewer can expand the stretches it collapses.
pub fn patch(path: &str, old: Option<&str>, new: Option<&str>) -> Option<String> {
    let old_lines = old.map(lines).unwrap_or_default();
    let new_lines = new.map(lines).unwrap_or_default();
    if old.is_some() == new.is_some() && old_lines == new_lines {
        return None;
    }

    let mut out = format!("diff --git a/{path} b/{path}\n");
    match (old, new) {
        (None, Some(_)) => out.push_str("new file mode 100644\n"),
        (Some(_), None) => out.push_str("deleted file mode 100644\n"),
        _ => {}
    }
    let side = |exists: bool, prefix: &str| {
        if exists {
            format!("{prefix}/{path}")
        } else {
            "/dev/null".to_string()
        }
    };
    out.push_str(&format!("--- {}\n+++ {}\n", side(old.is_some(), "a"), side(new.is_some(), "b")));
    if old_lines.is_empty() && new_lines.is_empty() {
        return Some(out);
    }

    // One hunk covering both versions whole.
    let range = |len: usize| if len == 0 { "0,0".to_string() } else { format!("1,{len}") };
    out.push_str(&format!("@@ -{} +{} @@\n", range(old_lines.len()), range(new_lines.len())));
    let mut push = |prefix: char, lines: &[&str]| {
        for line in lines {
            out.push(prefix);
            out.push_str(line);
            out.push('\n');
        }
    };
    for op in diff(&old_lines, &new_lines) {
        match op {
            DiffOp::Equal { old_index, len, .. } => push(' ', &old_lines[old_index..old_index + len]),
            DiffOp::Delete { old_index, old_len, .. } => push('-', &old_lines[old_index..old_index + old_len]),
            DiffOp::Insert { new_index, new_len, .. } => push('+', &new_lines[new_index..new_index + new_len]),
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                push('-', &old_lines[old_index..old_index + old_len]);
                push('+', &new_lines[new_index..new_index + new_len]);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(base: &str, text: &str) -> Vec<(MarkerKind, Range<usize>)> {
        markers(base, text).into_iter().map(|m| (m.kind, m.range)).collect()
    }

    #[test]
    fn remotes_map_to_their_websites() {
        let site = "https://github.com/owner/repo";
        assert_eq!(web_url("https://github.com/owner/repo.git").as_deref(), Some(site));
        assert_eq!(web_url("git@github.com:owner/repo.git").as_deref(), Some(site));
        assert_eq!(web_url("ssh://git@github.com:22/owner/repo").as_deref(), Some(site));
        assert_eq!(web_url("https://user@github.com/owner/repo/").as_deref(), Some(site));
        assert_eq!(web_url("C:/some/folder"), None);
        assert_eq!(percent_encode("a b#c.rs"), "a%20b%23c.rs");
    }

    #[test]
    fn line_endings_dont_count_as_changes() {
        assert!(markers("a\nb\n", "a\r\nb\r\n").is_empty());
    }

    #[test]
    fn marks_added_modified_and_deleted_lines() {
        assert_eq!(kinds("a\nb\n", "a\nb\nc\n"), [(MarkerKind::Added, 4..6)]);
        assert_eq!(kinds("a\nb\n", "a\nB\n"), [(MarkerKind::Modified, 2..4)]);
        assert_eq!(kinds("a\nb\nc\n", "a\nc\n"), [(MarkerKind::Deleted, 2..2)]);
        assert_eq!(kinds("a\nb\n", "a\n"), [(MarkerKind::Deleted, 2..2)]);
        assert_eq!(kinds("", "x"), [(MarkerKind::Added, 0..1)]);
    }

    #[test]
    fn patch_counts_match_its_lines() {
        let patch = patch("f.txt", Some("a\nb\nc\n"), Some("a\nB\nc\nd\n")).unwrap();
        assert!(patch.contains("@@ -1,3 +1,4 @@\n a\n-b\n+B\n c\n+d\n"), "{patch}");
        assert!(super::patch("f.txt", Some("a\n"), Some("a\r\n")).is_none());

        let added = super::patch("new.txt", None, Some("x\n")).unwrap();
        assert!(added.contains("--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,1 @@\n+x\n"), "{added}");
        let parsed = gpui_kit::component::diff::DiffFile::parse(&added).unwrap();
        assert_eq!(parsed[0].additions(), 1);

        let binary = file_patch("image.png", Some(vec![0, 159]), Some(vec![0, 146])).unwrap();
        assert!(gpui_kit::component::diff::DiffFile::parse(&binary).is_ok(), "{binary}");
        assert!(file_patch("same.bin", Some(vec![0, 159]), Some(vec![0, 159])).is_none());
    }
}
