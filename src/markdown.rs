//! Where the Markdown preview's links and images lead. Relative ones are
//! files next to the Markdown file, as on GitHub.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use gpui_kit::{Image, ImageFormat, ImageSource, SharedUri};

/// Where a link in a Markdown file leads.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// Anything with a scheme (`https:`, `mailto:`), for the system to open.
    Url,
    /// A heading in the same file.
    Anchor,
    /// A file or folder on disk.
    Path(PathBuf),
}

/// Images embedded in a file as `data:` URLs, decoded once rather than on
/// every frame. `None` for one that couldn't be decoded.
pub type EmbeddedImages = Arc<Mutex<HashMap<SharedUri, Option<Arc<Image>>>>>;

/// Resolves one Markdown file's links and images.
#[derive(Clone)]
pub struct Links {
    /// The file's folder, which relative links start from.
    folder: PathBuf,
    /// Where links starting with `/` start: the repository, as on GitHub.
    root: PathBuf,
    embedded: EmbeddedImages,
}

impl Links {
    pub fn new(file: &Path, root: &Path, embedded: EmbeddedImages) -> Self {
        Self {
            folder: file.parent().unwrap_or(root).to_path_buf(),
            root: root.to_path_buf(),
            embedded,
        }
    }

    pub fn target(&self, url: &str) -> Target {
        let url = url.trim();
        if url.is_empty() || url.starts_with('#') {
            return Target::Anchor;
        }
        if let Some(rest) = url.strip_prefix("file://") {
            return Target::Path(file_url_path(rest));
        }
        if has_scheme(url) {
            return Target::Url;
        }
        // A `#heading` or `?query` after a file name doesn't name the file.
        let path = percent_decode(url.split(['#', '?']).next().unwrap_or_default());
        let path = match path.strip_prefix('/') {
            Some(rest) => self.root.join(rest),
            // Absolute on this system, like `C:\notes.md` on Windows.
            None if Path::new(&path).is_absolute() => PathBuf::from(path),
            None => self.folder.join(path),
        };
        Target::Path(normalize(&path))
    }

    /// What to draw for an image: a file on disk, a decoded `data:` URL, or
    /// a web address.
    pub fn image(&self, uri: &SharedUri) -> ImageSource {
        if uri.starts_with("data:") {
            let mut embedded = self.embedded.lock().unwrap_or_else(PoisonError::into_inner);
            let image = embedded
                .entry(uri.clone())
                .or_insert_with(|| decode_data_url(uri))
                .clone();
            return match image {
                Some(image) => ImageSource::Image(image),
                None => uri.clone().into(),
            };
        }
        match self.target(uri) {
            Target::Path(path) => path.into(),
            Target::Url | Target::Anchor => uri.clone().into(),
        }
    }
}

/// Whether `url` starts with a scheme like `https:`. A Windows drive letter
/// (`C:`) isn't one.
fn has_scheme(url: &str) -> bool {
    url.split_once(':').is_some_and(|(scheme, _)| {
        scheme.len() > 1
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// The path in a `file://` URL, given what follows the `file://`.
fn file_url_path(rest: &str) -> PathBuf {
    let path = percent_decode(rest.split(['#', '?']).next().unwrap_or_default());
    // `file:///C:/notes.md` names `C:/notes.md` on Windows.
    let bytes = path.as_bytes();
    if cfg!(windows)
        && bytes.len() >= 3
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && bytes[2] == b':'
    {
        return PathBuf::from(&path[1..]);
    }
    PathBuf::from(path)
}

/// `%20` and the like, decoded. Text that wouldn't decode to UTF-8 is kept.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            decoded.push(byte);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).unwrap_or_else(|_| text.to_string())
}

/// `path` without `.` and `..` steps, so `docs/../README.md` opens as
/// `README.md`. Doesn't touch the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if normal.file_name().is_some() => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

fn decode_data_url(url: &str) -> Option<Arc<Image>> {
    let data = data_url::DataUrl::process(url).ok()?;
    let mime = data.mime_type();
    let format = ImageFormat::from_mime_type(&format!("{}/{}", mime.type_, mime.subtype))?;
    let (bytes, _) = data.decode_to_vec().ok()?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{EmbeddedImages, Links, Target};

    fn links() -> (Links, PathBuf) {
        let root = std::env::temp_dir().join("repo");
        let file = root.join("docs").join("guide.md");
        (Links::new(&file, &root, EmbeddedImages::default()), root)
    }

    #[test]
    fn relative_links_start_next_to_the_file() {
        let (links, root) = links();
        let docs = root.join("docs");
        assert_eq!(links.target("setup.md"), Target::Path(docs.join("setup.md")));
        assert_eq!(links.target("./setup.md#install"), Target::Path(docs.join("setup.md")));
        assert_eq!(links.target("../README.md"), Target::Path(root.join("README.md")));
        assert_eq!(links.target("/src/main.rs"), Target::Path(root.join("src").join("main.rs")));
        assert_eq!(links.target("my%20notes.md?plain=1"), Target::Path(docs.join("my notes.md")));
    }

    #[test]
    fn other_links_are_left_to_the_system() {
        let (links, _) = links();
        assert_eq!(links.target("https://example.com/a.md"), Target::Url);
        assert_eq!(links.target("mailto:someone@example.com"), Target::Url);
        assert_eq!(links.target("#usage"), Target::Anchor);
        // A bad escape stays as it was.
        assert_eq!(super::percent_decode("100%-done%zz"), "100%-done%zz");
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_are_paths() {
        let (links, _) = links();
        assert_eq!(links.target(r"C:\notes\todo.md"), Target::Path(Path::new(r"C:\notes\todo.md").into()));
        assert_eq!(
            links.target("file:///C:/notes/to%20do.md"),
            Target::Path(Path::new("C:/notes/to do.md").into())
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn file_urls_are_paths() {
        let (links, _) = links();
        assert_eq!(links.target("file:///home/me/to%20do.md"), Target::Path(Path::new("/home/me/to do.md").into()));
    }
}
