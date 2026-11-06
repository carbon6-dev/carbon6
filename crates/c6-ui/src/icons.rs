//! Line icons from Lucide (lucide.dev, ISC licence), their path data copied inline,
//! and small app icons drawn on the same grid. Replace emoji with drawn SVGs;
//! keep the icons, their positions and their accompanying labels.

use leptos::prelude::*;

/// A 16px icon in the text's colour: Lucide's 24-unit grid, stroked to
/// about 1.5px like the app's own.
fn icon(paths: impl IntoView + 'static) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="inline-block size-4 max-w-full shrink-0 align-text-bottom" fill="none" stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            {paths}
        </svg>
    }
}

/// The same icon as markup, for HTML built as a string (markdown, doc chips).
/// Rendering a view to HTML needs leptos's `ssr` feature, which the browser
/// build lacks: there `.to_html()` panics and breaks the page.
fn icon_html(paths: &str) -> String {
    format!(
        "<svg viewBox=\"0 0 24 24\" class=\"inline-block size-4 max-w-full shrink-0 align-text-bottom\" fill=\"none\" stroke=\"currentColor\" \
         stroke-width=\"2.25\" stroke-linecap=\"round\" stroke-linejoin=\"round\" aria-hidden=\"true\">{paths}</svg>"
    )
}

/// Icons used in HTML strings, as markup.
pub mod html {
    pub fn calendar() -> String {
        super::icon_html(r#"<rect x="3" y="5" width="18" height="16" rx="2"></rect><path d="M16 3v4M8 3v4M3 11h18M8 15h2m4 0h2"></path>"#)
    }

    pub fn file() -> String {
        super::icon_html(r#"<path d="M14 3H5v18h14V8ZM14 3v5h5M8 12h8M8 16h6"></path>"#)
    }

    pub fn chevron_down() -> String {
        super::icon_html(r#"<path d="m6 9 6 6 6-6"></path>"#)
    }

    pub fn arrow_up_right() -> String {
        super::icon_html(r#"<path d="M7 17 17 7M7 7h10v10"></path>"#)
    }
}

/// Lucide `file-diff`: a session's changes.
pub fn file_diff() -> impl IntoView {
    icon(view! {
        <path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z" />
        <path d="M9 10h6" />
        <path d="M12 13V7" />
        <path d="M9 17h6" />
    })
}

/// Lucide `square-terminal`: a session's terminal.
pub fn square_terminal() -> impl IntoView {
    icon(view! {
        <path d="m7 11 2-2-2-2" />
        <path d="M11 13h4" />
        <rect width="18" height="18" x="3" y="3" rx="2" ry="2" />
    })
}

/// App icons, on the same 24-unit grid as the line icons above.
pub fn bell() -> impl IntoView {
    icon(view! { <path d="M18 8a6 6 0 0 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9M10 21h4" /> })
}

pub fn bell_off() -> impl IntoView {
    icon(view! { <path d="m3 3 18 18M10 21h4M6 6c0 9-3 9-3 11h14M10 2.3A6 6 0 0 1 18 8c0 2 .3 4 .8 5" /> })
}

pub fn brain() -> impl IntoView {
    icon(
        view! { <path d="M12 5a3 3 0 0 0-6 0 4 4 0 0 0-3 6 4 4 0 0 0 1 7 4 4 0 0 0 8 1V5Zm0 0a3 3 0 0 1 6 0 4 4 0 0 1 3 6 4 4 0 0 1-1 7 4 4 0 0 1-8 1M6 5v3m12-3v3M4 12h3m13 0h-3M8 16v3m8-3v3" /> },
    )
}

pub fn grid_plus() -> impl IntoView {
    icon(view! { <rect x="3" y="3" width="18" height="18" rx="2" /><path d="M12 3v18M3 12h18M6 7h3M7.5 5.5v3" /> })
}

pub fn info() -> impl IntoView {
    icon(view! { <circle cx="12" cy="12" r="9" /><path d="M12 11v6m0-10h.01" /> })
}

pub fn help() -> impl IntoView {
    icon(view! { <circle cx="12" cy="12" r="9" /><path d="M9 9a3 3 0 0 1 6 0c0 2-3 2-3 4m0 4h.01" /> })
}

pub fn user() -> impl IntoView {
    icon(view! { <circle cx="12" cy="7" r="4" /><path d="M4 21v-2a8 8 0 0 1 16 0v2" /> })
}

pub fn plus() -> impl IntoView {
    icon(view! { <path d="M12 5v14M5 12h14" /> })
}

pub fn stop() -> impl IntoView {
    icon(view! { <rect x="5" y="5" width="14" height="14" rx="1" fill="currentColor" /> })
}

pub fn quote() -> impl IntoView {
    icon(view! { <path d="M3 12h6v6H3v-6c0-4 2-6 6-6M15 12h6v6h-6v-6c0-4 2-6 6-6" /> })
}

pub fn list() -> impl IntoView {
    icon(view! { <path d="M8 6h13M8 12h13M8 18h13M3 6h.01M3 12h.01M3 18h.01" /> })
}

pub fn list_ordered() -> impl IntoView {
    icon(view! { <path d="M9 6h12M9 12h12M9 18h12M3 3h1v6M3 9h2M3 14a1 1 0 0 1 2 0c0 1-2 2-2 3h2M3 20h2" /> })
}

/// App code draws it as `html::calendar` (in doc chips); tests compare the two.
#[cfg(test)]
pub fn calendar() -> impl IntoView {
    icon(view! { <rect x="3" y="5" width="18" height="16" rx="2" /><path d="M16 3v4M8 3v4M3 11h18M8 15h2m4 0h2" /> })
}

pub fn layout_template() -> impl IntoView {
    icon(view! { <rect x="3" y="3" width="18" height="18" rx="2" /><path d="M3 9h18M9 9v12" /> })
}

pub fn file() -> impl IntoView {
    icon(view! { <path d="M14 3H5v18h14V8ZM14 3v5h5M8 12h8M8 16h6" /> })
}

pub fn key() -> impl IntoView {
    icon(view! { <circle cx="8" cy="8" r="5" /><path d="m12 12 9 9m-3-3 3-3m-6 0 3-3" /> })
}

pub fn check() -> impl IntoView {
    icon(view! { <path d="m5 12 4 4L19 6" /> })
}

pub fn warning() -> impl IntoView {
    icon(view! { <path d="m12 3 10 18H2ZM12 9v4m0 4h.01" /> })
}

pub fn hourglass() -> impl IntoView {
    icon(view! { <path d="M5 3h14M5 21h14M7 3v5l10 8v5M17 3v5L7 16v5" /> })
}

pub fn play() -> impl IntoView {
    icon(view! { <path d="m7 4 14 8-14 8Z" /> })
}

pub fn circle() -> impl IntoView {
    icon(view! { <circle cx="12" cy="12" r="6" /> })
}

pub fn chevron_down() -> impl IntoView {
    icon(view! { <path d="m6 9 6 6 6-6" /> })
}

pub fn arrow_up() -> impl IntoView {
    icon(view! { <path d="M12 21V3m-7 7 7-7 7 7" /> })
}

pub fn arrow_down() -> impl IntoView {
    icon(view! { <path d="M12 3v18m-7-7 7 7 7-7" /> })
}

pub fn arrow_left() -> impl IntoView {
    icon(view! { <path d="M21 12H3m7 7-7-7 7-7" /> })
}

pub fn arrow_right() -> impl IntoView {
    icon(view! { <path d="M3 12h18m-7-7 7 7-7 7" /> })
}

pub fn arrow_up_right() -> impl IntoView {
    icon(view! { <path d="M7 17 17 7M7 7h10v10" /> })
}

pub fn arrow_down_right() -> impl IntoView {
    icon(view! { <path d="m7 7 10 10M7 17h10V7" /> })
}

pub fn corner_down_right() -> impl IntoView {
    icon(view! { <path d="M3 3v9a3 3 0 0 0 3 3h15m-6-6 6 6-6 6" /> })
}

pub fn undo() -> impl IntoView {
    icon(view! { <path d="M3 8h11a7 7 0 0 1 0 14M3 8l6-6m-6 6 6 6" /> })
}

pub fn rotate() -> impl IntoView {
    icon(view! { <path d="M3 10a9 9 0 1 1 1 7M3 3v7h7" /> })
}

pub fn pencil() -> impl IntoView {
    icon(view! { <path d="m15 4 5 5L9 20l-6 1 1-6ZM12 7l5 5" /> })
}

pub fn message() -> impl IntoView {
    icon(view! { <path d="M21 15a3 3 0 0 1-3 3H8l-5 3V6a3 3 0 0 1 3-3h12a3 3 0 0 1 3 3Z" /> })
}

pub fn bookmark() -> impl IntoView {
    icon(view! { <path d="M6 3h12v18l-6-4-6 4Z" /> })
}

pub fn link() -> impl IntoView {
    icon(view! { <path d="m9 15 6-6m-5-3 2-2a5 5 0 0 1 8 8l-2 2M6 10l-2 2a5 5 0 0 0 8 8l2-2" /> })
}

pub fn paperclip() -> impl IntoView {
    icon(view! { <path d="m8 13 7-7a3 3 0 0 1 4 4l-9 9a5 5 0 0 1-7-7l10-10m-3 9-5 5" /> })
}

pub fn lock() -> impl IntoView {
    icon(view! { <rect x="5" y="10" width="14" height="11" rx="2" /><path d="M8 10V6a4 4 0 0 1 8 0v4" /> })
}

pub fn menu() -> impl IntoView {
    icon(view! { <path d="M4 6h16M4 12h16M4 18h16" /> })
}

pub fn trash() -> impl IntoView {
    icon(view! { <path d="M3 6h18M9 6V3h6v3M5 6l1 15h12l1-15M10 10v7m4-7v7" /> })
}

pub fn mail() -> impl IntoView {
    icon(view! { <rect x="3" y="5" width="18" height="14" rx="2" /><path d="m3 6 9 7 9-7" /> })
}

pub fn diamond() -> impl IntoView {
    icon(view! { <path d="m12 3 9 9-9 9-9-9Z" /> })
}

pub fn zap() -> impl IntoView {
    icon(view! { <path d="m13 2-9 12h7l-1 8 10-13h-8Z" /> })
}

pub fn arrows_left_right() -> impl IntoView {
    icon(view! { <path d="M3 7h18m-4-4 4 4-4 4M21 17H3m4-4-4 4 4 4" /> })
}

pub fn chart() -> impl IntoView {
    icon(view! { <path d="M3 3v18h18M7 15l4-6 4 3 6-8" /> })
}

pub fn reaction() -> impl IntoView {
    icon(view! { <path d="M21 13a9 9 0 1 1-10-10M8 14a4 4 0 0 0 8 0M8 8h.01M14 8h.01M19 2v6m-3-3h6" /> })
}

/// Lucide `folder`: a session's files.
pub fn folder() -> impl IntoView {
    icon(view! {
        <path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z" />
    })
}

/// Lucide `package`: a session's outputs.
pub fn package() -> impl IntoView {
    icon(view! {
        <path d="M11 21.73a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16V8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73z" />
        <path d="M12 22V12" />
        <path d="m3.3 7 7.703 4.734a2 2 0 0 0 1.994 0L20.7 7" />
        <path d="m7.5 4.27 9 5.15" />
    })
}

/// Lucide `chevron-left`: back.
pub fn chevron_left() -> impl IntoView {
    icon(view! { <path d="m15 18-6-6 6-6" /> })
}

/// Lucide `chevron-right`: forward.
pub fn chevron_right() -> impl IntoView {
    icon(view! { <path d="m9 18 6-6-6-6" /> })
}

/// Lucide `x`: close.
pub fn x() -> impl IntoView {
    icon(view! {
        <path d="M18 6 6 18" />
        <path d="m6 6 12 12" />
    })
}

/// Lucide `ellipsis`: a menu of actions.
pub fn ellipsis() -> impl IntoView {
    icon(view! {
        <circle cx="12" cy="12" r="1" />
        <circle cx="19" cy="12" r="1" />
        <circle cx="5" cy="12" r="1" />
    })
}

/// Lucide `star`, filled when `on` (pinned).
pub fn star(on: bool) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-4 shrink-0" fill=if on { "currentColor" } else { "none" } stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <polygon points="12 2 15.09 8.26 22 9.27 17 14.14 18.18 21.02 12 17.77 5.82 21.02 7 14.14 2 9.27 8.91 8.26 12 2" />
        </svg>
    }
}

/// Lucide `square-arrow-out-up-right`: into a tile of its own.
pub fn square_arrow_out() -> impl IntoView {
    icon(view! {
        <path d="M21 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h6" />
        <path d="m21 3-9 9" />
        <path d="M15 3h6v6" />
    })
}

/// Lucide `locate-fixed`: show an anchor in its document.
pub fn locate_fixed() -> impl IntoView {
    icon(view! {
        <path d="M2 12h3m14 0h3M12 2v3m0 14v3" />
        <circle cx="12" cy="12" r="7" />
        <circle cx="12" cy="12" r="3" />
    })
}

/// Lucide `minus`: minimize.
pub fn minus() -> impl IntoView {
    icon(view! { <path d="M5 12h14" /> })
}

/// Lucide `maximize`: focus (fill the screen).
pub fn maximize() -> impl IntoView {
    icon(view! {
        <path d="M8 3H5a2 2 0 0 0-2 2v3" />
        <path d="M21 8V5a2 2 0 0 0-2-2h-3" />
        <path d="M3 16v3a2 2 0 0 0 2 2h3" />
        <path d="M16 21h3a2 2 0 0 0 2-2v-3" />
    })
}

/// Lucide `minimize`: out of focus, back to the tiles.
pub fn minimize() -> impl IntoView {
    icon(view! {
        <path d="M8 3v3a2 2 0 0 1-2 2H3" />
        <path d="M21 8h-3a2 2 0 0 1-2-2V3" />
        <path d="M3 16h3a2 2 0 0 1 2 2v3" />
        <path d="M16 21v-3a2 2 0 0 1 2-2h3" />
    })
}

/// Lucide `search`: a search field's magnifier.
pub fn search() -> impl IntoView {
    icon(view! {
        <circle cx="11" cy="11" r="8" />
        <path d="m21 21-4.3-4.3" />
    })
}

/// Lucide `sliders-horizontal`: filter, sort and group.
pub fn sliders() -> impl IntoView {
    icon(view! {
        <path d="M21 4h-7M10 4H3M21 12h-9M8 12H3M21 20h-5M12 20H3" />
        <path d="M14 2v4M8 10v4M16 18v4" />
    })
}

/// Lucide `sparkles`, small: a name suggested by a model.
pub fn sparkles() -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-3.5 shrink-0" fill="none" stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z" />
            <path d="M20 2v4" />
            <path d="M22 4h-4" />
            <circle cx="4" cy="20" r="2" />
        </svg>
    }
}

/// Lucide `square-check` when `on`, else `square`: a toggle chip's box.
pub fn check_box(on: bool) -> impl IntoView {
    view! {
        <svg viewBox="0 0 24 24" class="size-3.5 shrink-0" fill="none" stroke="currentColor" stroke-width="2.25"
            stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect width="18" height="18" x="3" y="3" rx="2" />
            {on.then(|| view! { <path d="m9 12 2 2 4-4" /> })}
        </svg>
    }
}

/// An icon button in a bar (a tile's, a pane's, a session's strip): 28px
/// round a 16px glyph, 40px on a touch screen. Hover only with a mouse (a
/// tapped button on a phone keeps no look); a ring for the keyboard's focus.
pub const BAR_BUTTON: &str = "flex size-7 shrink-0 items-center justify-center rounded text-zinc-600 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 disabled:opacity-30 pointer-fine:hover:bg-black/10 pointer-fine:hover:text-zinc-900 pointer-fine:disabled:hover:bg-transparent pointer-coarse:size-10 dark:text-zinc-400 dark:pointer-fine:hover:bg-white/10 dark:pointer-fine:hover:text-zinc-100";

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    // These are text, not pictograms: punctuation, keyboard shortcuts,
    // directions in prose and the worker/SSH relationship separator.
    fn icon_glyph(c: char) -> bool {
        if "↑→↓↵⇄⇧⌘⌥─".contains(c) {
            return false;
        }
        matches!(c as u32, 0x203c | 0x2049 | 0x20e3 | 0x2100..=0x2bff | 0x3030 | 0x303d | 0x3297 | 0x3299 | 0xfe0e..=0xfe0f | 0xff0b | 0x1f000..=0x1faff)
    }

    pub(crate) fn assert_no_icon_glyphs(markup: &str) {
        let found: Vec<char> = markup.chars().filter(|c| icon_glyph(*c)).collect();
        assert!(found.is_empty(), "app-owned markup contains icon glyphs {found:?}: {markup}");
    }

    #[test]
    fn the_guard_distinguishes_chrome_icons_from_typography() {
        for c in "🔔🧠📄🔑📦⚠✓✕⏳💬★▶".chars() {
            assert!(icon_glyph(c), "missed {c}");
        }
        assert_no_icon_glyphs("Back › ‹ … · × — – “text” · ⌘/Ctrl ⇧ ↵ · Settings → Memex · worker ⇄ host");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn restored_icons_share_the_accessible_svg_grid() {
        for mark in [
            bell_off().into_any(),
            brain().into_any(),
            grid_plus().into_any(),
            info().into_any(),
            help().into_any(),
            user().into_any(),
            plus().into_any(),
            stop().into_any(),
            quote().into_any(),
            list().into_any(),
            list_ordered().into_any(),
            calendar().into_any(),
            layout_template().into_any(),
            arrow_left().into_any(),
            arrow_right().into_any(),
            arrow_up_right().into_any(),
            arrow_down_right().into_any(),
            corner_down_right().into_any(),
            locate_fixed().into_any(),
            undo().into_any(),
        ] {
            let html = mark.to_html();
            assert_eq!(html.matches("<svg").count(), 1, "{html}");
            for attr in ["viewBox=\"0 0 24 24\"", "stroke=\"currentColor\"", "aria-hidden=\"true\"", "stroke-linecap=\"round\""] {
                assert!(html.contains(attr), "{attr} missing: {html}");
            }
            assert_no_icon_glyphs(&html);
        }
    }

    /// The markup copies draw what the components draw.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn markup_icons_match_their_components() {
        let norm = |s: String| s.replace("<!>", "").replace("/>", ">");
        for (markup, view) in [
            (html::calendar(), calendar().into_any()),
            (html::file(), file().into_any()),
            (html::chevron_down(), chevron_down().into_any()),
            (html::arrow_up_right(), arrow_up_right().into_any()),
        ] {
            let drawn = view.to_html();
            for part in markup.split(['<', '>']).filter(|p| p.contains("d=\"") || p.contains("rect ")) {
                assert!(norm(drawn.clone()).contains(part.trim_end_matches('/').trim()), "{part} missing from {drawn}");
            }
            assert!(markup.starts_with("<svg viewBox=\"0 0 24 24\"") && markup.contains("aria-hidden=\"true\""), "{markup}");
        }
    }

    /// `.to_html()` panics in the browser build (no `ssr` feature), so app
    /// code builds HTML strings from `icons::html`, never from a view.
    #[test]
    fn app_code_never_renders_views_to_html() {
        fn check(path: &Path) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    check(&entry.unwrap().path());
                }
                return;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                return;
            }
            let src = std::fs::read_to_string(path).unwrap();
            let app = src.split("#[cfg(test)]").next().unwrap_or("");
            for (n, line) in app.lines().enumerate() {
                let code = line.split("//").next().unwrap_or("");
                assert!(
                    !code.contains(".to_html()"),
                    "{}:{}: .to_html() breaks the page in the browser; use icons::html",
                    path.display(),
                    n + 1
                );
            }
        }
        check(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    }

    #[test]
    fn app_chrome_uses_svg_not_symbol_icons() {
        fn check(path: &Path) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    check(&entry.unwrap().path());
                }
                return;
            }
            if !matches!(path.extension().and_then(|s| s.to_str()), Some("rs" | "html" | "js")) {
                return;
            }
            // Slack's emoji names/table are imported message and reaction
            // content, not icons belonging to carbon6's interface.
            if path.ends_with("chat/text.rs") {
                return;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let production = source.split("#[cfg(test)]").next().unwrap();
            for (i, line) in production.lines().enumerate() {
                if ["//", "/*", "*", "<!--"].iter().any(|prefix| line.trim_start().starts_with(prefix)) {
                    continue;
                }
                // Directions between words are prose; action arrows must
                // stay visible too, drawn as SVG rather than text glyphs.
                assert!(!line.contains("→\""), "{}:{}: draw the trailing action arrow with icons.rs", path.display(), i + 1);
                if let Some(c) = line.chars().find(|c| icon_glyph(*c)) {
                    panic!("{}:{}: {c} is an icon glyph; replace it with an icons.rs SVG, not words alone", path.display(), i + 1);
                }
            }
        }
        let ui = Path::new(env!("CARGO_MANIFEST_DIR"));
        for owned in ["src", "js", "pwa", "index.html"] {
            check(&ui.join(owned));
        }
        // vendor/xterm.js contains Unicode width/terminal data. Like
        // terminal output itself, it must still support user text.
    }
}
