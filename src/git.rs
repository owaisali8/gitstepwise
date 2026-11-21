//! Git integration through the `git` command line: the committed version of
//! a file, the repository's changed files, and line-level diffs between the
//! committed text and the text being edited.
//!
//! Everything here blocks on `git`, so callers run it on a background thread.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
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

    /// A file's contents in the last commit, by its path relative to the root,
    /// or `None` if it isn't in the last commit.
    pub fn committed_bytes(&self, relative: &str) -> Option<Vec<u8>> {
        run(git(&self.root).arg("show").arg(format!("HEAD:{relative}")))
    }

    /// `path` relative to the root, with `/` separators.
    pub fn relative(&self, path: &Path) -> Option<String> {
        let path = std::fs::canonicalize(path).map_or_else(|_| path.to_path_buf(), strip_verbatim_prefix);
        let relative = path.strip_prefix(&self.root).ok()?;
        let parts: Vec<_> = relative.iter().map(|part| part.to_string_lossy()).collect();
        Some(parts.join("/"))
    }
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
        _ => {
            let side = |exists: bool, prefix: &str| {
                if exists {
                    format!("{prefix}/{path}")
                } else {
                    "/dev/null".to_string()
                }
            };
            Some(format!(
                "diff --git a/{path} b/{path}\nBinary files {} and {} differ\n",
                side(old.is_some(), "a"),
                side(new.is_some(), "b"),
            ))
        }
    }
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
