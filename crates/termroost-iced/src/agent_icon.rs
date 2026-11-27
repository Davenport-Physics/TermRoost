//! The logos of the coding agents, shown next to the sessions and terminals
//! that run one, and TerminalRooster's own icon for sessions that run none.
//! Where the agent logos come from: `icons/LICENSES.md`.

use std::sync::LazyLock;

use iced::widget::{Svg, svg};
use iced::{Color, Theme};
use termroost_core::AgentKind;

// Built once so every view hands iced the same handle, and its rasterized
// copy is reused instead of drawn again.
static CLAUDE: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(include_bytes!("../icons/claude.svg").as_slice()));
static CODEX: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(include_bytes!("../icons/codex.svg").as_slice()));
static GROK: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(include_bytes!("../icons/grok.svg").as_slice()));
static ANTIGRAVITY: LazyLock<svg::Handle> =
    LazyLock::new(|| svg::Handle::from_memory(include_bytes!("../icons/antigravity.svg").as_slice()));
// The app icon only exists as PNGs. Wrapped in an SVG, it goes through the same
// `svg` widget and cache as the logos, without iced's `image` feature.
static TERMINAL: LazyLock<svg::Handle> = LazyLock::new(|| {
    let png = include_bytes!("../../../packaging/icons/terminalrooster-48.png");
    let svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="48" height="48" viewBox="0 0 48 48"><image width="48" height="48" xlink:href="data:image/png;base64,{}"/></svg>"#,
        base64(png)
    );
    svg::Handle::from_memory(svg.into_bytes())
});

/// The logo of `kind`, `size` pixels square. `ink` is the color of the text
/// around it, which one-color logos are drawn in.
pub fn agent_icon<'a>(kind: AgentKind, size: f32, ink: impl Fn(&Theme) -> Color + 'a) -> Svg<'a> {
    let handle = match kind {
        AgentKind::Claude => &CLAUDE,
        AgentKind::Codex => &CODEX,
        AgentKind::Grok => &GROK,
        AgentKind::Antigravity => &ANTIGRAVITY,
    };
    let icon = svg(LazyLock::force(handle).clone()).width(size).height(size);
    match kind {
        // Grok's logo is plain black or white, so it can't keep a color of its
        // own on both light and dark backgrounds.
        AgentKind::Grok => icon.style(move |t, _| svg::Style { color: Some(ink(t)) }),
        AgentKind::Claude | AgentKind::Codex | AgentKind::Antigravity => icon,
    }
}

/// TerminalRooster's own icon, `size` pixels square, for plain terminals.
pub fn terminal_icon<'a>(size: f32) -> Svg<'a> {
    svg(LazyLock::force(&TERMINAL).clone()).width(size).height(size)
}

/// Standard base64 (RFC 4648), with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            // `chunk.len()` bytes make `chunk.len() + 1` characters; `=` pads the rest.
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648() {
        let examples = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in examples {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0x00]), "//4A");
    }

    #[test]
    fn agent_svgs_parse() {
        let icons: &[(&str, &[u8])] = &[
            ("claude", include_bytes!("../icons/claude.svg")),
            ("codex", include_bytes!("../icons/codex.svg")),
            ("grok", include_bytes!("../icons/grok.svg")),
            ("antigravity", include_bytes!("../icons/antigravity.svg")),
        ];
        let opts = usvg::Options::default();
        for (name, bytes) in icons {
            let tree = usvg::Tree::from_data(bytes, &opts).unwrap_or_else(|e| panic!("{name}.svg failed: {e}"));
            assert!(tree.size().width() > 0.0 && tree.size().height() > 0.0, "{name}.svg empty size");
        }
    }
}
