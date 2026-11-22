//! The edit timeline: every change to the repository's files, in the order
//! it happened, so edits made one after another (say, by a coding agent)
//! can be reviewed as separate steps rather than one diff per file.
//!
//! A file watcher notices each write. A step's "before" is what the file
//! held after its previous step, or in the last commit, so editing one file,
//! then another, then the first again makes three steps. The timeline is
//! kept in `.git/gitstepwise/timeline.json` and starts afresh after each
//! commit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt as _;
use futures::channel::mpsc::unbounded;
// Not a glob: gpui's `test` macro would replace `#[test]` in the tests below.
use gpui_kit::{AppContext as _, Context, Task};
use notify::{EventKind, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};

use crate::git::{self, Repo};

/// After a write, wait this long for the writes that come with it.
const SETTLE: Duration = Duration::from_millis(150);
/// Writes to the same file this close together are one step: formatters
/// and agents often write a file twice in a row.
const MERGE_WINDOW_MS: u64 = 2000;
/// Larger files are compared by a hash of their bytes, not shown as text.
const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024;
const SAVE_DELAY: Duration = Duration::from_millis(200);
const FILE_VERSION: u32 = 1;

/// A file's contents at one point in the timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "lowercase")]
pub enum Content {
    Missing,
    Text(Arc<str>),
    /// Not text (or too large to show): a hash of the bytes.
    Binary(u64),
}

impl Content {
    fn from_bytes(bytes: Option<Vec<u8>>) -> Self {
        let Some(bytes) = bytes else {
            return Self::Missing;
        };
        let bytes = if bytes.len() <= MAX_TEXT_BYTES {
            match String::from_utf8(bytes) {
                Ok(text) => return Self::Text(text.into()),
                Err(err) => err.into_bytes(),
            }
        } else {
            bytes
        };
        // FNV-1a: stable across runs and Rust versions, unlike `DefaultHasher`.
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
        Self::Binary(hash)
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    fn exists(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    /// Equal for review purposes: line endings don't count.
    fn same_as(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => git::same_lines(a, b),
            _ => self == other,
        }
    }
}

/// Where a step's change came from.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Written by another program: an agent, a formatter, a script.
    Disk,
    /// Saved from this editor.
    Editor,
    /// Changed while the editor was closed. One step per file.
    WhileClosed,
}

/// One change to one file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Step {
    pub id: u64,
    /// Relative to the repository's root, with `/` separators.
    pub path: String,
    pub before: Content,
    pub after: Content,
    /// Milliseconds since the Unix epoch.
    pub time: u64,
    pub source: Source,
    pub reviewed: bool,
}

impl Step {
    /// A one-file patch from `before` to `after`, or `None` if no lines changed.
    pub fn patch(&self) -> Option<String> {
        match (&self.before, &self.after) {
            (Content::Binary(_), _) | (_, Content::Binary(_)) => Some(git::binary_patch(
                &self.path,
                self.before.exists(),
                self.after.exists(),
            )),
            (before, after) => git::patch(&self.path, before.text(), after.text()),
        }
    }
}

/// The timeline as kept in `.git`.
#[derive(Default, Serialize, Deserialize)]
struct State {
    version: u32,
    /// The commit the timeline started from; a new commit starts a new one.
    head: Option<String>,
    next_id: u64,
    steps: Vec<Step>,
    /// What each file touched since the timeline began held last: the
    /// "before" of its next step. Files not here are as in `head`.
    known: HashMap<String, Content>,
}

impl State {
    /// Start afresh from the files as they are now.
    fn baseline(repo: &Repo) -> Self {
        let known = repo
            .changed_paths()
            .into_iter()
            .filter_map(|path| {
                let content = read(repo, &path)?;
                Some((path, content))
            })
            .collect();
        Self {
            version: FILE_VERSION,
            head: repo.head(),
            next_id: 1,
            steps: Vec::new(),
            known,
        }
    }

    /// The saved timeline, with what changed while the editor was closed
    /// added; or a fresh one if there's none, or it's from an older commit.
    fn load(repo: &Repo) -> Self {
        // Before the app was renamed, the timeline lived under the old name.
        let saved: Option<Self> = std::fs::read(state_file(repo))
            .or_else(|_| std::fs::read(repo.git_dir.join(OLD_STATE_FOLDER).join(STATE_FILE)))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let mut state = match saved {
            Some(saved) if saved.version == FILE_VERSION && saved.head == repo.head() => saved,
            _ => return Self::baseline(repo),
        };

        let mut paths = repo.changed_paths();
        paths.extend(state.known.keys().cloned());
        paths.sort();
        paths.dedup();
        let time = now_ms();
        for path in paths {
            let Some(after) = read(repo, &path) else {
                continue;
            };
            let before = match state.known.get(&path) {
                Some(content) => content.clone(),
                None => Content::from_bytes(repo.committed_bytes(&path)),
            };
            if !before.same_as(&after) {
                state.steps.push(Step {
                    id: state.next_id,
                    path: path.clone(),
                    before,
                    after: after.clone(),
                    time,
                    source: Source::WhileClosed,
                    reviewed: false,
                });
                state.next_id += 1;
            }
            state.known.insert(path, after);
        }
        state
    }
}

/// The timeline's folder in `.git`, and where it was as "GPUI Editor".
const STATE_FOLDER: &str = "gitstepwise";
const OLD_STATE_FOLDER: &str = "gpui-editor";
const STATE_FILE: &str = "timeline.json";

fn state_file(repo: &Repo) -> PathBuf {
    repo.git_dir.join(STATE_FOLDER).join(STATE_FILE)
}

/// A file's contents now. `None` if it can't be read at the moment (say, a
/// folder, or a file its writer has locked), unlike `Some(Missing)` when it's gone.
fn read(repo: &Repo, path: &str) -> Option<Content> {
    match std::fs::read(repo.root.join(path)) {
        Ok(bytes) => Some(Content::from_bytes(Some(bytes))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(Content::Missing),
        Err(_) => None,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// "just now", "3 min ago", and so on.
pub fn ago(time_ms: u64) -> String {
    let seconds = now_ms().saturating_sub(time_ms) / 1000;
    match seconds {
        0..=9 => "just now".into(),
        10..=59 => format!("{seconds} s ago"),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86_399 => format!("{} h ago", seconds / 3600),
        86_400..=2_591_999 => format!("{} d ago", seconds / 86_400),
        2_592_000..=31_535_999 => format!("{} mo ago", seconds / 2_592_000),
        _ => format!("{} y ago", seconds / 31_536_000),
    }
}

/// A changed path, as read off the main thread.
struct Found {
    path: String,
    after: Content,
    /// The committed contents, for a file the timeline hasn't seen yet.
    committed: Option<Content>,
}

/// Read the changed files, leaving out what Git ignores (build output, say).
fn inspect(repo: &Repo, paths: Vec<String>, unseen: HashSet<String>) -> Vec<Found> {
    let ignored = repo.ignored(&paths);
    paths
        .into_iter()
        .filter(|path| !ignored.contains(path))
        .filter_map(|path| {
            let after = read(repo, &path)?;
            let committed = unseen
                .contains(&path)
                .then(|| Content::from_bytes(repo.committed_bytes(&path)));
            Some(Found { path, after, committed })
        })
        .collect()
}

pub struct Timeline {
    repo: Repo,
    loaded: bool,
    state: State,
    /// Contents this editor just saved, so the watcher can tell the editor's
    /// own writes from other programs'.
    saved_here: HashMap<String, Content>,
    watch_error: Option<String>,
    _watcher: Option<notify::RecommendedWatcher>,
    _watch_task: Option<Task<()>>,
    _state_task: Option<Task<()>>,
    _head_task: Option<Task<()>>,
    _save_task: Option<Task<()>>,
}

impl Timeline {
    pub fn new(repo: Repo, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            repo,
            loaded: false,
            state: State::default(),
            saved_here: HashMap::new(),
            watch_error: None,
            _watcher: None,
            _watch_task: None,
            _state_task: None,
            _head_task: None,
            _save_task: None,
        };
        // Watch first, so nothing written while the timeline loads is missed.
        this.watch(cx);
        this.replace_state(false, cx);
        this
    }

    pub fn root(&self) -> &Path {
        &self.repo.root
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded
    }

    pub fn watch_error(&self) -> Option<&str> {
        self.watch_error.as_deref()
    }

    pub fn steps(&self) -> &[Step] {
        &self.state.steps
    }

    /// The editor is about to write `text` to `path`.
    pub fn note_saved(&mut self, path: &Path, text: &str) {
        if let Some(path) = self.repo.relative(path) {
            self.saved_here.insert(path, Content::Text(text.into()));
        }
    }

    pub fn set_reviewed(&mut self, id: u64, reviewed: bool, cx: &mut Context<Self>) {
        if let Some(step) = self.state.steps.iter_mut().find(|step| step.id == id) {
            step.reviewed = reviewed;
            self.save(cx);
            cx.notify();
        }
    }

    pub fn clear_reviewed(&mut self, cx: &mut Context<Self>) {
        for step in &mut self.state.steps {
            step.reviewed = false;
        }
        self.save(cx);
        cx.notify();
    }

    /// Forget the steps so far and start again from the files as they are.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.replace_state(true, cx);
    }

    /// After something changed in `.git`: a new commit starts a new timeline.
    pub fn check_head(&mut self, cx: &mut Context<Self>) {
        if !self.loaded {
            return;
        }
        let repo = self.repo.clone();
        let head = self.state.head.clone();
        self._head_task = Some(cx.spawn(async move |this, cx| {
            let new_head = cx.background_spawn(async move { repo.head() }).await;
            if new_head != head {
                this.update(cx, |this, cx| this.replace_state(true, cx)).ok();
            }
        }));
    }

    fn replace_state(&mut self, fresh: bool, cx: &mut Context<Self>) {
        self.loaded = false;
        cx.notify();
        let repo = self.repo.clone();
        self._state_task = Some(cx.spawn(async move |this, cx| {
            let state = cx
                .background_spawn(async move {
                    if fresh {
                        State::baseline(&repo)
                    } else {
                        State::load(&repo)
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.state = state;
                this.saved_here.clear();
                this.loaded = true;
                this.save(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn watch(&mut self, cx: &mut Context<Self>) {
        let (sender, mut receiver) = unbounded::<PathBuf>();
        let root = self.repo.root.clone();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else {
                return;
            };
            if matches!(event.kind, EventKind::Access(_)) {
                return;
            }
            for path in event.paths {
                let _ = sender.unbounded_send(path);
            }
        })
        .and_then(|mut watcher| {
            watcher.watch(&root, RecursiveMode::Recursive)?;
            Ok(watcher)
        });
        match watcher {
            Ok(watcher) => self._watcher = Some(watcher),
            Err(err) => {
                self.watch_error = Some(err.to_string());
                return;
            }
        }

        self._watch_task = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = receiver.next().await {
                let mut batch = vec![first];
                cx.background_executor().timer(SETTLE).await;
                while let Ok(path) = receiver.try_recv() {
                    batch.push(path);
                }
                // Hold the writes until the timeline has loaded.
                loop {
                    match this.read_with(cx, |this, _| this.loaded) {
                        Ok(true) => break,
                        Ok(false) => cx.background_executor().timer(Duration::from_millis(100)).await,
                        Err(_) => return,
                    }
                }
                let Ok((repo, paths, unseen)) = this.read_with(cx, |this, _| this.prepare(batch)) else {
                    return;
                };
                if paths.is_empty() {
                    continue;
                }
                let found = cx
                    .background_spawn(async move { inspect(&repo, paths, unseen) })
                    .await;
                if this.update(cx, |this, cx| this.apply(found, cx)).is_err() {
                    return;
                }
            }
        }));
    }

    /// The batch's paths relative to the root, once each, in the order they
    /// were first written; and which of them the timeline hasn't seen.
    fn prepare(&self, batch: Vec<PathBuf>) -> (Repo, Vec<String>, HashSet<String>) {
        let mut seen = HashSet::new();
        let paths: Vec<String> = batch
            .iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(&self.repo.root).ok()?;
                let parts: Vec<String> = relative
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect();
                if parts.is_empty() || parts[0] == ".git" {
                    return None;
                }
                Some(parts.join("/"))
            })
            .filter(|path| seen.insert(path.clone()))
            .collect();
        let unseen = paths
            .iter()
            .filter(|path| !self.state.known.contains_key(*path))
            .cloned()
            .collect();
        (self.repo.clone(), paths, unseen)
    }

    fn apply(&mut self, found: Vec<Found>, cx: &mut Context<Self>) {
        let mut changed = false;
        for Found { path, after, committed } in found {
            // `known` may have moved on since the batch was read.
            let Some(before) = self.state.known.get(&path).cloned().or(committed) else {
                continue;
            };
            if before.same_as(&after) {
                self.state.known.insert(path, after);
                continue;
            }
            let source = if self.saved_here.get(&path) == Some(&after) {
                self.saved_here.remove(&path);
                Source::Editor
            } else {
                Source::Disk
            };
            self.push(path.clone(), before, after.clone(), source);
            self.state.known.insert(path, after);
            changed = true;
        }
        if changed {
            self.save(cx);
            cx.notify();
        }
    }

    /// Add a step, or extend the last one if it's the same file written
    /// again moments later.
    fn push(&mut self, path: String, before: Content, after: Content, source: Source) {
        let time = now_ms();
        let steps = &mut self.state.steps;
        if let Some(last) = steps.last_mut()
            && last.path == path
            && last.source == source
            && time.saturating_sub(last.time) < MERGE_WINDOW_MS
        {
            last.after = after;
            last.time = time;
            last.reviewed = false;
            // Written and then put back: no step at all.
            if last.before.same_as(&last.after) {
                steps.pop();
            }
            return;
        }
        steps.push(Step {
            id: self.state.next_id,
            path,
            before,
            after,
            time,
            source,
            reviewed: false,
        });
        self.state.next_id += 1;
    }

    /// Write the timeline to `.git`, shortly, off the main thread.
    fn save(&mut self, cx: &mut Context<Self>) {
        let state = State {
            version: FILE_VERSION,
            head: self.state.head.clone(),
            next_id: self.state.next_id,
            steps: self.state.steps.clone(),
            known: self.state.known.clone(),
        };
        let file = state_file(&self.repo);
        self._save_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_DELAY).await;
            cx.background_spawn(async move {
                if let Some(folder) = file.parent() {
                    let _ = std::fs::create_dir_all(folder);
                }
                if let Ok(json) = serde_json::to_vec(&state) {
                    let _ = std::fs::write(&file, json);
                }
            })
            .await;
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_compares_lines_not_line_endings() {
        let lf = Content::from_bytes(Some(b"a\nb\n".to_vec()));
        let crlf = Content::from_bytes(Some(b"a\r\nb\r\n".to_vec()));
        assert!(lf.same_as(&crlf));
        assert!(!lf.same_as(&Content::Missing));
        assert!(matches!(Content::from_bytes(Some(vec![0, 159])), Content::Binary(_)));
    }

    #[test]
    fn step_patches_show_only_that_edit() {
        let step = Step {
            id: 1,
            path: "a.txt".into(),
            before: Content::Text("one\ntwo\n".into()),
            after: Content::Text("one\nTWO\n".into()),
            time: 0,
            source: Source::Disk,
            reviewed: false,
        };
        let patch = step.patch().unwrap();
        assert!(patch.contains("-two\n+TWO\n"), "{patch}");
        let saved = serde_json::to_string(&step).unwrap();
        let loaded: Step = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded.after, step.after);
    }
}
