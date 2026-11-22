//! Icons for files in the explorer, chosen by name and then by extension,
//! with a color for each kind so they're told apart at a glance.

use gpui_kit::assets::IconName as LucideIcon;

/// RGB colors for a dark and a light theme.
pub type Colors = (u32, u32);

const RUST: Colors = (0xdea584, 0xb7410e);
const TYPESCRIPT: Colors = (0x4f9ee8, 0x2563a8);
const JAVASCRIPT: Colors = (0xe8d44d, 0x9a7d00);
const PYTHON: Colors = (0x5a9fd4, 0x2f6c9f);
const GO: Colors = (0x00add8, 0x007d9c);
const C_FAMILY: Colors = (0x6295cb, 0x3a6ea5);
const JVM: Colors = (0xe76f00, 0xb35600);
const HTML: Colors = (0xe5734a, 0xc2410c);
const CSS: Colors = (0x42a5f5, 0x1d6fb8);
const JSON: Colors = (0xcbcb41, 0x8a7f00);
const CONFIG: Colors = (0xa0a0a0, 0x707070);
const YAML: Colors = (0xe06c75, 0xb3363f);
const MARKDOWN: Colors = (0x519aba, 0x2f6f9f);
const SHELL: Colors = (0x89e051, 0x3f8f1f);
const IMAGE: Colors = (0xa074c4, 0x7b4fa3);
const ARCHIVE: Colors = (0xc09553, 0x8a6232);
const SPREADSHEET: Colors = (0x4caf50, 0x2e7d32);
const LOCK: Colors = (0xe5c07b, 0xa67c00);
const NODE: Colors = (0x8bc34a, 0x558b2f);
const GIT: Colors = (0xf05133, 0xc73a1f);
const LICENSE: Colors = (0xd4b84a, 0x8f7a1e);
const DATABASE: Colors = (0xd19a66, 0x9a5b20);

/// The icon for a file named `name`, and its colors; `None` for a plain
/// file, which uses the default icon in the theme's muted color.
pub fn file_icon(name: &str) -> Option<(LucideIcon, Colors)> {
    use LucideIcon::*;
    let name = name.to_ascii_lowercase();
    let by_name = match name.as_str() {
        "cargo.lock" | "package-lock.json" | "yarn.lock" | "pnpm-lock.yaml" | "poetry.lock"
        | "composer.lock" | "gemfile.lock" | "uv.lock" | "bun.lockb" => Some((FileLock, LOCK)),
        "cargo.toml" | "pyproject.toml" | "go.mod" => Some((Package, RUST)),
        "package.json" => Some((Package, NODE)),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" | ".git-blame-ignore-revs" => {
            Some((GitBranch, GIT))
        }
        "dockerfile" | ".dockerignore" | ".editorconfig" => Some((FileCog, CONFIG)),
        "makefile" | "justfile" => Some((FileTerminal, SHELL)),
        _ if name.starts_with("readme") => Some((BookOpen, MARKDOWN)),
        _ if ["license", "licence", "copying"].iter().any(|prefix| name.starts_with(prefix)) => {
            Some((Scale, LICENSE))
        }
        _ if name == ".env" || name.starts_with(".env.") => Some((FileKey, LOCK)),
        _ => None,
    };
    if by_name.is_some() {
        return by_name;
    }

    let extension = name.rsplit_once('.').map(|(_, extension)| extension)?;
    Some(match extension {
        "rs" => (FileCode, RUST),
        "ts" | "tsx" | "mts" | "cts" => (FileCode, TYPESCRIPT),
        "js" | "jsx" | "mjs" | "cjs" => (FileCode, JAVASCRIPT),
        "py" | "pyw" | "pyi" => (FileCode, PYTHON),
        "go" => (FileCode, GO),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "cs" => (FileCode, C_FAMILY),
        "java" | "kt" | "kts" | "scala" => (FileCode, JVM),
        "html" | "htm" | "xml" | "vue" | "svelte" => (FileCode, HTML),
        "css" | "scss" | "sass" | "less" => (FileCode, CSS),
        "json" | "jsonc" | "json5" => (FileBraces, JSON),
        "toml" | "ini" | "cfg" | "conf" | "rc" | "properties" => (FileCog, CONFIG),
        "yaml" | "yml" => (FileCog, YAML),
        "md" | "markdown" | "mdx" | "rst" => (FileText, MARKDOWN),
        "sh" | "bash" | "zsh" | "fish" | "ps1" | "psm1" | "bat" | "cmd" => (FileTerminal, SHELL),
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "ico" | "webp" | "bmp" | "avif" => (FileImage, IMAGE),
        "zip" | "tar" | "gz" | "tgz" | "7z" | "rar" | "xz" | "bz2" | "zst" => (FileArchive, ARCHIVE),
        "csv" | "tsv" | "xls" | "xlsx" | "ods" => (FileSpreadsheet, SPREADSHEET),
        "lock" => (FileLock, LOCK),
        "sql" | "db" | "sqlite" | "sqlite3" => (Database, DATABASE),
        "pem" | "key" | "crt" | "pub" => (FileKey, LOCK),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn icon(name: &str) -> Option<LucideIcon> {
        file_icon(name).map(|(icon, _)| icon)
    }

    #[test]
    fn names_come_before_extensions() {
        assert_eq!(icon("Cargo.lock"), Some(LucideIcon::FileLock));
        assert_eq!(icon("Cargo.toml"), Some(LucideIcon::Package));
        assert_eq!(icon("config.toml"), Some(LucideIcon::FileCog));
        assert_eq!(icon("README.md"), Some(LucideIcon::BookOpen));
        assert_eq!(icon("notes.md"), Some(LucideIcon::FileText));
        assert_eq!(icon("main.RS"), Some(LucideIcon::FileCode));
        assert_eq!(icon(".gitignore"), Some(LucideIcon::GitBranch));
        assert_eq!(icon("data.bin"), None);
        assert_eq!(icon("Makefile"), Some(LucideIcon::FileTerminal));
    }
}
