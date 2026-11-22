//! The small icon before each file and folder name: a glyph for the kind of
//! file (source code, data, settings, an image…) in a color for its language
//! or tool, so a folder can be read at a glance.
//!
//! The colors are the hues of the interface theme's terminal colors: the only
//! hues a theme names besides its accent and states, picked to read on its
//! background, so a custom theme's icons follow it.
//!
//! The glyphs were drawn for Terminal Rooster (see `icons/LICENSES.md`), on
//! the same 16 px grid as the interface icons.

use std::sync::LazyLock;

use iced::widget::{Svg, svg};
use iced::{Color, Theme};

use crate::theme;
use crate::themes::ThemeDef;

/// What an icon shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    File,
    Text,
    Code,
    Braces,
    Config,
    Markdown,
    Image,
    Lock,
    Shell,
    Git,
    Database,
    Archive,
    Folder,
    FolderOpen,
}

/// The files, in the order of [`Glyph`].
const SOURCES: [&[u8]; 14] = [
    include_bytes!("../icons/files/file.svg"),
    include_bytes!("../icons/files/text.svg"),
    include_bytes!("../icons/files/code.svg"),
    include_bytes!("../icons/files/braces.svg"),
    include_bytes!("../icons/files/config.svg"),
    include_bytes!("../icons/files/markdown.svg"),
    include_bytes!("../icons/files/image.svg"),
    include_bytes!("../icons/files/lock.svg"),
    include_bytes!("../icons/files/shell.svg"),
    include_bytes!("../icons/files/git.svg"),
    include_bytes!("../icons/files/database.svg"),
    include_bytes!("../icons/files/archive.svg"),
    include_bytes!("../icons/files/folder.svg"),
    include_bytes!("../icons/files/folder_open.svg"),
];

// Built once so every view hands iced the same handles, and their rasterized
// copies are reused instead of drawn again.
static HANDLES: LazyLock<Vec<svg::Handle>> =
    LazyLock::new(|| SOURCES.iter().map(|bytes| svg::Handle::from_memory(*bytes)).collect());

/// The color of an icon: one of the theme's hues, or the muted text color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tint {
    Muted,
    Red,
    Orange,
    Yellow,
    Green,
    Cyan,
    Blue,
    Magenta,
}

/// An icon: what it shows, in which color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look(pub Glyph, pub Tint);

/// The hues of a theme: red, green, yellow, blue, magenta and cyan of its
/// terminal colors.
#[derive(Clone, Copy, Debug)]
pub struct Hues([Color; 6]);

impl Hues {
    pub fn of(def: &ThemeDef) -> Hues {
        Hues(std::array::from_fn(|i| def.terminal.ansi[i + 1].color()))
    }

    fn color(self, tint: Tint, t: &Theme) -> Color {
        let [red, green, yellow, blue, magenta, cyan] = self.0;
        match tint {
            Tint::Muted => theme::muted(t),
            Tint::Red => red,
            // Terminal colors have no orange.
            Tint::Orange => theme::mix(red, yellow, 0.6),
            Tint::Yellow => yellow,
            Tint::Green => green,
            Tint::Cyan => cyan,
            Tint::Blue => blue,
            Tint::Magenta => magenta,
        }
    }
}

/// The icon of a folder.
pub fn folder(open: bool) -> Look {
    Look(if open { Glyph::FolderOpen } else { Glyph::Folder }, Tint::Muted)
}

/// The icon of a file named `name`: by its whole name for files tools know
/// by name (`Cargo.toml`, `.gitignore`, `LICENSE`), else by its extension.
/// Case doesn't matter.
pub fn file(name: &str) -> Look {
    use Glyph::*;
    use Tint::*;
    let name = name.to_lowercase();
    // The part after the last dot; `.bashrc` has none, `site.tar.gz` is a `gz`.
    let ext = match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    };
    if let Some(look) = by_name(&name, ext) {
        return look;
    }
    match ext {
        "rs" => Look(Code, Orange),
        "py" | "pyi" | "pyw" => Look(Code, Blue),
        "js" | "mjs" | "cjs" => Look(Code, Yellow),
        "ts" | "mts" | "cts" => Look(Code, Blue),
        "jsx" | "tsx" => Look(Code, Cyan),
        "go" | "dart" | "elm" | "groovy" => Look(Code, Cyan),
        "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "m" | "mm" => Look(Code, Blue),
        "lua" | "r" | "pl" | "pm" | "nim" | "css" => Look(Code, Blue),
        "cs" | "fs" | "fsx" | "vb" | "kt" | "kts" | "php" | "ex" | "exs" | "hs" | "jl" => Look(Code, Magenta),
        "scss" | "sass" | "less" => Look(Code, Magenta),
        "java" | "scala" | "rb" | "erl" | "hrl" => Look(Code, Red),
        "swift" | "zig" | "ml" | "mli" | "svelte" => Look(Code, Orange),
        "html" | "htm" | "xml" | "xaml" | "xsl" | "plist" => Look(Code, Orange),
        "vue" | "clj" | "cljs" | "glsl" | "hlsl" | "wgsl" => Look(Code, Green),
        "json" | "jsonc" | "json5" | "jsonl" => Look(Braces, Yellow),
        "ipynb" => Look(Braces, Orange),
        "graphql" | "gql" => Look(Braces, Magenta),
        "proto" => Look(Braces, Cyan),
        "toml" | "ini" | "cfg" | "conf" | "properties" | "env" => Look(Config, Muted),
        "yaml" | "yml" => Look(Config, Magenta),
        "md" | "markdown" | "mdx" => Look(Markdown, Blue),
        "txt" | "log" | "rst" | "adoc" | "org" => Look(Text, Muted),
        "csv" | "tsv" => Look(Text, Green),
        "pdf" => Look(Text, Red),
        "diff" | "patch" => Look(Git, Green),
        "png" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "bmp" | "ico" | "icns" | "tif" | "tiff" | "avif"
        | "heic" | "tga" | "qoi" | "pnm" | "pbm" | "pgm" | "ppm" | "pam" => Look(Image, Magenta),
        "svg" | "svgz" => Look(Image, Yellow),
        "lock" => Look(Lock, Muted),
        "sh" | "bash" | "zsh" | "fish" => Look(Shell, Green),
        "ps1" | "psm1" | "psd1" => Look(Shell, Blue),
        "bat" | "cmd" => Look(Shell, Yellow),
        "sql" | "db" | "sqlite" | "sqlite3" => Look(Database, Magenta),
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "jar" | "whl" | "deb" | "rpm"
        | "crate" => Look(Archive, Yellow),
        // Dotfiles such as `.editorconfig` set something up.
        "" if name.starts_with('.') => Look(Config, Muted),
        _ => Look(File, Muted),
    }
}

/// Files known by their name; `name` is lowercase.
fn by_name(name: &str, ext: &str) -> Option<Look> {
    use Glyph::*;
    use Tint::*;
    let look = match name {
        // Package manifests, in their language's color.
        "cargo.toml" => Look(Config, Orange),
        "package.json" => Look(Braces, Green),
        "pyproject.toml" | "requirements.txt" | "pipfile" => Look(Config, Blue),
        "go.mod" | "go.sum" => Look(Config, Cyan),
        "gemfile" | "rakefile" => Look(Config, Red),
        "package-lock.json" | "npm-shrinkwrap.json" | "pnpm-lock.yaml" | "bun.lockb" => Look(Lock, Muted),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" | ".mailmap" | ".git-blame-ignore-revs" => {
            Look(Git, Orange)
        }
        "dockerfile"
        | "containerfile"
        | ".dockerignore"
        | "docker-compose.yml"
        | "docker-compose.yaml"
        | "compose.yml"
        | "compose.yaml" => Look(Config, Blue),
        "makefile" | "gnumakefile" | "justfile" | "cmakelists.txt" => Look(Config, Orange),
        ".bashrc" | ".bash_profile" | ".bash_logout" | ".profile" | ".zshrc" | ".zprofile" => Look(Shell, Green),
        ".env" => Look(Config, Yellow),
        _ if name.starts_with(".env.") => Look(Config, Yellow),
        _ if name.starts_with("dockerfile.") || ext == "dockerfile" => Look(Config, Blue),
        _ if name.starts_with("tsconfig") && ext == "json" => Look(Config, Blue),
        // `LICENSE`, `LICENSE-MIT`, `COPYING.txt`; not `license_check.py`.
        _ if matches!(name.split(['.', '-', '_']).next(), Some("license" | "licence" | "copying" | "notice"))
            && matches!(ext, "" | "md" | "txt") =>
        {
            Look(Text, Yellow)
        }
        _ => return None,
    };
    Some(look)
}

/// `look`, `size` pixels square, in its color from `hues`. `dim` fades it, as
/// the name next to it is when git ignores the file.
pub fn icon<'a>(look: Look, hues: Hues, dim: bool, size: f32) -> Svg<'a> {
    let Look(glyph, tint) = look;
    svg(HANDLES[glyph as usize].clone()).width(size).height(size).style(move |t, _| {
        let color = hues.color(tint, t);
        svg::Style { color: Some(if dim { color.scale_alpha(0.5) } else { color }) }
    })
}

#[cfg(test)]
mod tests {
    use super::Glyph::*;
    use super::Tint::*;
    use super::*;

    #[test]
    fn files_are_known_by_their_extension_in_any_case() {
        assert_eq!(file("main.rs"), Look(Code, Orange));
        assert_eq!(file("App.TSX"), Look(Code, Cyan));
        assert_eq!(file("README.md"), Look(Markdown, Blue));
        assert_eq!(file("site.tar.gz"), Look(Archive, Yellow), "the last extension counts");
        assert_eq!(file("notes"), Look(File, Muted));
        assert_eq!(file("data.xyz"), Look(File, Muted));
    }

    #[test]
    fn some_files_are_known_by_their_whole_name() {
        assert_eq!(file("Cargo.toml"), Look(Config, Orange));
        assert_eq!(file("rustfmt.toml"), Look(Config, Muted));
        assert_eq!(file("Cargo.lock"), Look(Lock, Muted));
        assert_eq!(file("package-lock.json"), Look(Lock, Muted), "not a plain .json");
        assert_eq!(file("Dockerfile"), Look(Config, Blue));
        assert_eq!(file("Makefile"), Look(Config, Orange));
    }

    #[test]
    fn dotfiles() {
        assert_eq!(file(".gitignore"), Look(Git, Orange));
        assert_eq!(file(".env"), Look(Config, Yellow));
        assert_eq!(file(".env.local"), Look(Config, Yellow));
        assert_eq!(file(".bashrc"), Look(Shell, Green));
        assert_eq!(file(".editorconfig"), Look(Config, Muted));
        assert_eq!(file(".eslintrc.json"), Look(Braces, Yellow), "a dotfile with an extension goes by it");
    }

    #[test]
    fn licenses() {
        for name in ["LICENSE", "LICENSE-MIT", "LICENSE.md", "COPYING.txt", "licence"] {
            assert_eq!(file(name), Look(Text, Yellow), "{name}");
        }
        assert_eq!(file("license_check.py"), Look(Code, Blue));
    }

    #[test]
    fn hues_come_from_the_terminal_colors() {
        let def = ThemeDef::dark();
        let hues = Hues::of(&def);
        let t = def.iced_theme();
        assert_eq!(hues.color(Red, &t), def.terminal.ansi[1].color());
        assert_eq!(hues.color(Cyan, &t), def.terminal.ansi[6].color());
        assert_eq!(hues.color(Muted, &t), theme::muted(&t));
    }
}
