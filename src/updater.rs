//! Updates: the newest release of the app on GitHub, and putting it in
//! place of the running copy.
//!
//! `.github/workflows/release.yml` publishes each version as a GitHub
//! release with the app attached. An update downloads that file next to the
//! running app, checks it against the size and SHA-256 digest GitHub lists
//! for it, and swaps it in. Windows can rename a running program but not
//! overwrite it, so the running file is renamed out of the way first (to
//! `gitstepwise.exe.old`) and removed on the next start. The new version
//! runs from the next start, or straight away with Restart to Update.
//!
//! Release builds check a few seconds after starting and then every few
//! hours, and install what they find, unless Help → Update Automatically is
//! off. Debug builds check only when asked, and never replace themselves.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use gpui_kit::*;
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The repository the releases come from, set in Cargo.toml.
pub const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// This copy's version, from Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// How long after starting the first automatic check waits, and how often
/// the app checks after that.
const FIRST_CHECK: Duration = Duration::from_secs(10);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
const API_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Where the Update Automatically setting is kept, in the config folder.
const SETTINGS_FILE: &str = "updates.json";

/// A version published on GitHub.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: Version,
    /// The release's page on GitHub, with its notes.
    pub page: String,
    /// The app for this system, if the release has one.
    pub file: Option<ReleaseFile>,
}

/// The app's file attached to a release.
#[derive(Clone, Debug, PartialEq)]
pub struct ReleaseFile {
    name: String,
    url: String,
    size: u64,
    /// Lowercase hex, when GitHub lists one.
    sha256: Option<String>,
}

/// Where an update stands.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// Not checking, and nothing newer found.
    Idle,
    /// Asking GitHub. `asked` when the user asked, so it shows.
    Checking { asked: bool },
    /// A newer version that isn't installed: this copy can't install it, or
    /// the user said not now, or installing it failed.
    Available(Release),
    Downloading(Version),
    /// The new version is in place, and runs from the next start.
    Installed(Version),
}

pub enum UpdaterEvent {
    /// The user asked and a newer version is there: offer to install it.
    Found(Release),
    /// Something to say in the status bar.
    Status { message: String, is_error: bool },
}

/// Checks for updates, on a schedule or when asked, and installs them.
pub struct Updater {
    state: UpdateState,
    /// Help → Update Automatically: check on a schedule, and install what's found.
    automatic: bool,
    /// The running program, which an update replaces.
    exe: Option<PathBuf>,
    _task: Option<Task<()>>,
    _schedule: Option<Task<()>>,
}

impl EventEmitter<UpdaterEvent> for Updater {}

impl Updater {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let exe = std::env::current_exe().ok();
        if let Some(exe) = exe.clone() {
            cx.background_spawn(async move { clean_up(&exe) }).detach();
        }
        let mut this = Self {
            state: UpdateState::Idle,
            automatic: Settings::load().automatic,
            exe,
            _task: None,
            _schedule: None,
        };
        this.schedule(cx);
        this
    }

    pub fn state(&self) -> &UpdateState {
        &self.state
    }

    pub fn automatic(&self) -> bool {
        self.automatic
    }

    /// Whether this copy can replace itself with `release`. Debug builds
    /// never do: they're run from where Cargo builds them.
    pub fn can_install(&self, release: &Release) -> bool {
        !cfg!(debug_assertions) && self.exe.is_some() && release.file.is_some()
    }

    pub fn set_automatic(&mut self, automatic: bool, cx: &mut Context<Self>) {
        self.automatic = automatic;
        if let Err(err) = (Settings { automatic }).save() {
            self.status(format!("Couldn't save the update setting: {err}"), true, cx);
        }
        self.schedule(cx);
        cx.notify();
    }

    /// Check now and then, while updates are automatic. Only release builds
    /// check on their own.
    fn schedule(&mut self, cx: &mut Context<Self>) {
        let on = self.automatic && !cfg!(debug_assertions) && self.exe.is_some();
        self._schedule = on.then(|| {
            cx.spawn(async move |this, cx| {
                let mut wait = FIRST_CHECK;
                loop {
                    cx.background_executor().timer(wait).await;
                    if this.update(cx, |this, cx| this.check(false, cx)).is_err() {
                        break;
                    }
                    wait = CHECK_EVERY;
                }
            })
        });
    }

    /// Ask GitHub for the latest release. When the user `asked`, every
    /// outcome is reported, and a newer version is offered rather than
    /// installed straight away.
    pub fn check(&mut self, asked: bool, cx: &mut Context<Self>) {
        match &self.state {
            UpdateState::Checking { .. } | UpdateState::Downloading(_) => {
                if asked {
                    self.status("Already checking for updates.", false, cx);
                }
                return;
            }
            UpdateState::Installed(version) => {
                if asked {
                    let message = format!("Gitstepwise {version} is installed. Restart to use it.");
                    self.status(message, false, cx);
                }
                return;
            }
            UpdateState::Idle | UpdateState::Available(_) => {}
        }
        // A version already offered and turned down, or that failed to
        // install, isn't installed on a schedule again.
        let known = match &self.state {
            UpdateState::Available(release) => Some(release.version.clone()),
            _ => None,
        };
        self.state = UpdateState::Checking { asked };
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let release = cx.background_spawn(async { latest_release() }).await;
            this.update(cx, |this, cx| this.checked(release, asked, known, cx)).ok();
        }));
    }

    fn checked(
        &mut self,
        release: Result<Release, String>,
        asked: bool,
        known: Option<Version>,
        cx: &mut Context<Self>,
    ) {
        self.state = UpdateState::Idle;
        cx.notify();
        let release = match release {
            Ok(release) => release,
            Err(err) => {
                if asked {
                    self.status(err, true, cx);
                }
                return;
            }
        };
        let current = current_version();
        if release.version <= current {
            if asked {
                self.status(format!("Gitstepwise {current} is the latest version."), false, cx);
            }
            return;
        }

        if !self.can_install(&release) {
            if asked {
                let reason = if release.file.is_none() {
                    "It has no download for this system"
                } else {
                    "This is a debug build, which doesn't replace itself"
                };
                let message = format!("Gitstepwise {} is available. {reason}: see {}", release.version, release.page);
                self.status(message, false, cx);
            }
            self.state = UpdateState::Available(release);
        } else if asked {
            cx.emit(UpdaterEvent::Found(release));
        } else if known.as_ref() == Some(&release.version) {
            self.state = UpdateState::Available(release);
        } else {
            self.install(release, cx);
        }
    }

    /// Offer the newer version found again, or show its page when this copy
    /// can't install it.
    pub fn offer(&mut self, cx: &mut Context<Self>) {
        if let UpdateState::Available(release) = &self.state {
            if self.can_install(release) {
                cx.emit(UpdaterEvent::Found(release.clone()));
            } else {
                cx.open_url(&release.page);
            }
        }
    }

    /// The user said not now: keep the version showing as available.
    pub fn postpone(&mut self, release: Release, cx: &mut Context<Self>) {
        if matches!(self.state, UpdateState::Idle) {
            self.state = UpdateState::Available(release);
            cx.notify();
        }
    }

    /// Download `release` and put it in place of the running copy.
    pub fn install(&mut self, release: Release, cx: &mut Context<Self>) {
        if !matches!(self.state, UpdateState::Idle | UpdateState::Available(_)) {
            return;
        }
        let (Some(file), Some(exe)) = (release.file.clone(), self.exe.clone()) else {
            return;
        };
        let version = release.version.clone();
        self.state = UpdateState::Downloading(version.clone());
        cx.notify();
        self._task = Some(cx.spawn(async move |this, cx| {
            let installed = cx.background_spawn(async move { install(&file, &exe) }).await;
            this.update(cx, |this, cx| match installed {
                Ok(()) => {
                    this.state = UpdateState::Installed(version.clone());
                    this.status(format!("Gitstepwise {version} is installed. Restart to use it."), false, cx);
                }
                Err(err) => {
                    this.state = UpdateState::Available(release);
                    this.status(format!("Couldn't update to Gitstepwise {version}: {err}"), true, cx);
                }
            })
            .ok();
        }));
    }

    /// Start the installed version once the app has quit.
    pub fn relaunch_after_quit(&self) {
        if let Some(exe) = &self.exe {
            *RELAUNCH.lock().unwrap_or_else(PoisonError::into_inner) = Some(exe.clone());
        }
    }

    fn status(&mut self, message: impl Into<String>, is_error: bool, cx: &mut Context<Self>) {
        cx.emit(UpdaterEvent::Status {
            message: message.into(),
            is_error,
        });
    }
}

/// The program to start when the app's run loop ends, set by Restart to Update.
static RELAUNCH: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Start the new version if Restart to Update asked for it. `main` calls
/// this once the app has quit, after everything open has been saved.
pub fn relaunch_if_asked() {
    let exe = RELAUNCH.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(exe) = exe
        && let Err(err) = std::process::Command::new(&exe).spawn()
    {
        eprintln!("Couldn't start {}: {err}", exe.display());
    }
}

pub fn current_version() -> Version {
    Version::parse(VERSION).unwrap_or_else(|_| Version::new(0, 0, 0))
}

/// The page listing every release.
pub fn releases_page() -> String {
    format!("{}/releases", REPOSITORY.trim_end_matches('/'))
}

/// The settings kept in `updates.json`.
#[derive(Serialize, Deserialize)]
struct Settings {
    automatic: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { automatic: true }
    }
}

impl Settings {
    fn file() -> Option<PathBuf> {
        Some(dirs::config_dir()?.join(crate::projects::CONFIG_FOLDER).join(SETTINGS_FILE))
    }

    fn load() -> Self {
        Self::file()
            .and_then(|file| std::fs::read(file).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> std::io::Result<()> {
        let file = Self::file().ok_or_else(|| std::io::Error::other("there's no config folder"))?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(file, serde_json::to_vec_pretty(self)?)
    }
}

/// The latest release on GitHub. Blocks.
fn latest_release() -> Result<Release, String> {
    let slug = github_slug(REPOSITORY)
        .ok_or("Updates come from GitHub, but the repository in Cargo.toml isn't a GitHub one.")?;
    let url = format!("https://api.github.com/repos/{slug}/releases/latest");
    let mut response = ureq::get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", user_agent())
        .config()
        .https_only(true)
        .timeout_global(Some(API_TIMEOUT))
        .build()
        .call()
        .map_err(|err| match err {
            ureq::Error::StatusCode(404) => "No release has been published yet.".to_string(),
            ureq::Error::StatusCode(403 | 429) => {
                "GitHub is turning requests away for now. Try again later.".to_string()
            }
            ureq::Error::StatusCode(code) => format!("GitHub answered with error {code}."),
            err => format!("Couldn't reach GitHub: {err}"),
        })?;
    let json = response
        .body_mut()
        .read_to_vec()
        .map_err(|err| format!("Couldn't read GitHub's answer: {err}"))?;
    parse_release(&json, asset_suffix())
}

fn user_agent() -> String {
    format!("Gitstepwise/{VERSION}")
}

/// "owner/name" from a GitHub repository's address.
fn github_slug(url: &str) -> Option<&str> {
    let slug = url.strip_prefix("https://github.com/")?.trim_end_matches('/');
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    let (owner, name) = slug.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then_some(slug)
}

/// How a release's file for this system ends: the release workflow names
/// it `gitstepwise-v1.2.3-windows-x64.exe`. Only Windows is published.
fn asset_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("-windows-x64.exe"),
        _ => None,
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    /// Like `sha256:3f…`.
    #[serde(default)]
    digest: Option<String>,
}

/// A release from GitHub's JSON, with the file whose name ends in `suffix`.
fn parse_release(json: &[u8], suffix: Option<&str>) -> Result<Release, String> {
    let release: GitHubRelease =
        serde_json::from_slice(json).map_err(|err| format!("Couldn't read GitHub's answer: {err}"))?;
    let version = Version::parse(release.tag_name.trim_start_matches('v'))
        .map_err(|_| format!("The latest release, {}, isn't named as a version.", release.tag_name))?;
    let file = suffix
        .and_then(|suffix| release.assets.into_iter().find(|asset| asset.name.ends_with(suffix)))
        .map(|asset| ReleaseFile {
            sha256: asset
                .digest
                .as_deref()
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .map(str::to_ascii_lowercase),
            name: asset.name,
            url: asset.browser_download_url,
            size: asset.size,
        });
    Ok(Release {
        version,
        page: release.html_url,
        file,
    })
}

/// Download `file` next to `exe` and swap it in. Blocks.
fn install(file: &ReleaseFile, exe: &Path) -> Result<(), String> {
    let new = sibling(exe, "new");
    let swapped = download(file, &new).and_then(|()| {
        swap(exe, &new).map_err(|err| format!("Couldn't put it in place of {}: {err}", exe.display()))
    });
    if swapped.is_err() {
        std::fs::remove_file(&new).ok();
    }
    swapped
}

fn download(file: &ReleaseFile, to: &Path) -> Result<(), String> {
    let mut response = ureq::get(&file.url)
        .header("User-Agent", user_agent())
        .config()
        .https_only(true)
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .build()
        .call()
        .map_err(|err| format!("Couldn't download {}: {err}", file.name))?;
    let write_error = |err: std::io::Error| format!("Couldn't write {}: {err}", to.display());
    let mut out = std::fs::File::create(to).map_err(write_error)?;
    let mut reader = response.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut head = Vec::new();
    let mut size = 0u64;
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|err| format!("The download of {} stopped: {err}", file.name))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        if size > file.size {
            return Err(format!("{} is bigger than GitHub says it is.", file.name));
        }
        if head.len() < 2 {
            head.extend_from_slice(&buffer[..read.min(2 - head.len())]);
        }
        hasher.update(&buffer[..read]);
        out.write_all(&buffer[..read]).map_err(write_error)?;
    }
    out.sync_all().map_err(write_error)?;
    verify(file, size, &format!("{:x}", hasher.finalize()), &head)
}

/// Whether a download is the file GitHub lists: as big, with the same
/// digest (when GitHub gives one), and a program for this system.
fn verify(file: &ReleaseFile, size: u64, sha256: &str, head: &[u8]) -> Result<(), String> {
    if size != file.size {
        return Err(format!("{} came down incomplete.", file.name));
    }
    if file.sha256.as_deref().is_some_and(|expected| expected != sha256) {
        return Err(format!("{} doesn't match the digest GitHub lists for it.", file.name));
    }
    // Windows programs start with "MZ".
    if cfg!(windows) && head != b"MZ" {
        return Err(format!("{} isn't a Windows program.", file.name));
    }
    Ok(())
}

/// `exe` with `.extension` added: `gitstepwise.exe.new`.
fn sibling(exe: &Path, extension: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(extension);
    exe.with_file_name(name)
}

/// Put `new` in place of `exe`. The running file is renamed out of the way
/// first, since Windows can rename a running program but not overwrite it.
fn swap(exe: &Path, new: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    std::fs::set_permissions(new, std::fs::metadata(exe)?.permissions())?;

    // One left by an update before, unless a copy still runs from it.
    let mut old = sibling(exe, "old");
    if old.exists() && std::fs::remove_file(&old).is_err() {
        old = sibling(exe, &format!("old-{}", std::process::id()));
    }
    std::fs::rename(exe, &old)?;
    if let Err(err) = std::fs::rename(new, exe) {
        std::fs::rename(&old, exe).ok();
        return Err(err);
    }
    Ok(())
}

/// Remove what updates left next to `exe`: old versions, and a download
/// that didn't finish. Files still in use stay until the next time.
fn clean_up(exe: &Path) {
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    let name = name.to_string_lossy();
    let (old, new) = (format!("{name}.old"), format!("{name}.new"));
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if file_name.starts_with(&old) || *file_name == new {
            std::fs::remove_file(entry.path()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use semver::Version;

    use super::{ReleaseFile, clean_up, github_slug, parse_release, sibling, swap, verify};

    #[test]
    fn repositories_name_their_slug() {
        assert_eq!(github_slug("https://github.com/owner/app"), Some("owner/app"));
        assert_eq!(github_slug("https://github.com/owner/app.git/"), Some("owner/app"));
        assert_eq!(github_slug("https://gitlab.com/owner/app"), None);
        assert_eq!(github_slug("https://github.com/owner"), None);
        assert_eq!(github_slug("https://github.com/owner/app/tree/main"), None);
    }

    #[test]
    fn releases_are_read_from_github_json() {
        let json = br#"{
            "tag_name": "v1.2.3",
            "html_url": "https://github.com/owner/app/releases/tag/v1.2.3",
            "assets": [
                {"name": "app-v1.2.3-linux-x64", "browser_download_url": "https://x/linux", "size": 5},
                {"name": "app-v1.2.3-windows-x64.exe", "browser_download_url": "https://x/win",
                 "size": 7, "digest": "sha256:ABC123"}
            ]
        }"#;
        let release = parse_release(json, Some("-windows-x64.exe")).unwrap();
        assert_eq!(release.version, Version::new(1, 2, 3));
        assert_eq!(release.page, "https://github.com/owner/app/releases/tag/v1.2.3");
        let file = release.file.unwrap();
        assert_eq!((file.url.as_str(), file.size), ("https://x/win", 7));
        assert_eq!(file.sha256.as_deref(), Some("abc123"));

        // No file for this system, and a tag that isn't a version.
        assert_eq!(parse_release(json, None).unwrap().file, None);
        let json = br#"{"tag_name": "nightly", "html_url": "https://x", "assets": []}"#;
        assert!(parse_release(json, None).is_err());
    }

    #[test]
    fn downloads_must_match_what_github_lists() {
        let file = ReleaseFile {
            name: "app.exe".into(),
            url: String::new(),
            size: 10,
            sha256: Some("abc".into()),
        };
        let head: &[u8] = if cfg!(windows) { b"MZ" } else { b"\x7fE" };
        assert!(verify(&file, 10, "abc", head).is_ok());
        assert!(verify(&file, 9, "abc", head).is_err());
        assert!(verify(&file, 10, "abd", head).is_err());
        let unlisted = ReleaseFile { sha256: None, ..file.clone() };
        assert!(verify(&unlisted, 10, "anything", head).is_ok());
        if cfg!(windows) {
            assert!(verify(&file, 10, "abc", b"<h").is_err());
        }
    }

    #[test]
    fn the_new_version_takes_the_old_ones_place() {
        let dir = std::env::temp_dir().join(format!("gitstepwise-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe: PathBuf = dir.join("app.exe");
        let new = sibling(&exe, "new");
        std::fs::write(&exe, "old version").unwrap();
        std::fs::write(&new, "new version").unwrap();
        // Left by an earlier update.
        std::fs::write(sibling(&exe, "old"), "older version").unwrap();

        swap(&exe, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "new version");
        assert_eq!(std::fs::read_to_string(sibling(&exe, "old")).unwrap(), "old version");
        assert!(!new.exists());

        clean_up(&exe);
        assert!(!sibling(&exe, "old").exists());
        assert!(exe.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
