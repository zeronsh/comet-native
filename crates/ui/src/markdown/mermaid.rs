//! Isolated native Mermaid adapter. Call only on a background executor.
use crate::theme::Theme;
use std::sync::Mutex;

pub const ENGINE_VERSION: &str = "mermaid-rs-renderer/0.3.1";
pub const MAX_SOURCE_BYTES: usize = 16 * 1024;
// This backend has no cooperative cancellation. Bound inputs and serialize its
// CPU work across previews; UI owners discard results from superseded revisions.
static RENDER_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
pub struct Palette {
    dark: bool,
    font: String,
    background: String,
    raised: String,
    text: String,
    muted: String,
    border: String,
    accent: String,
}

fn color(color: gpui::Hsla, background: gpui::Hsla) -> String {
    let mut c = color.to_rgb();
    let bg = background.to_rgb();
    c.r = c.r * c.a + bg.r * (1.0 - c.a);
    c.g = c.g * c.a + bg.g * (1.0 - c.a);
    c.b = c.b * c.a + bg.b * (1.0 - c.a);
    format!(
        "#{:02x}{:02x}{:02x}",
        (c.r * 255.0).round() as u8,
        (c.g * 255.0).round() as u8,
        (c.b * 255.0).round() as u8
    )
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            dark: theme.appearance.is_dark(),
            font: theme.font_sans.to_string(),
            background: color(theme.surface, theme.bg),
            raised: color(theme.surface_raised, theme.bg),
            text: color(theme.text, theme.bg),
            muted: color(theme.text_muted, theme.bg),
            border: color(theme.border_strong, theme.bg),
            accent: color(theme.accent, theme.bg),
        }
    }
}

pub fn render(source: &str, palette: &Palette) -> Result<String, String> {
    if source.len() > MAX_SOURCE_BYTES
        || source.lines().count() > 256
        || source
            .split(|c: char| c.is_whitespace() || matches!(c, ';' | '>' | '{' | '}'))
            .count()
            > 2048
    {
        return Err("Diagram exceeds preview complexity limit".into());
    }
    let _guard = RENDER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::panic::catch_unwind(|| {
        let mut options = mermaid_rs_renderer::RenderOptions::default();
        options.theme = if palette.dark {
            mermaid_rs_renderer::Theme::dark()
        } else {
            mermaid_rs_renderer::Theme::modern()
        };
        let theme = &mut options.theme;
        theme.font_family = palette.font.clone();
        theme.font_size = 14.0;
        theme.background = palette.background.clone();
        theme.primary_color = palette.raised.clone();
        theme.primary_text_color = palette.text.clone();
        theme.primary_border_color = palette.border.clone();
        theme.text_color = palette.text.clone();
        theme.line_color = palette.muted.clone();
        theme.secondary_color = palette.raised.clone();
        theme.tertiary_color = palette.raised.clone();
        theme.edge_label_background = palette.background.clone();
        theme.cluster_background = palette.background.clone();
        theme.cluster_border = palette.border.clone();
        theme.sequence_actor_fill = palette.raised.clone();
        theme.sequence_actor_border = palette.border.clone();
        theme.sequence_actor_line = palette.muted.clone();
        theme.sequence_note_fill = palette.raised.clone();
        theme.sequence_note_border = palette.border.clone();
        theme.sequence_activation_fill = palette.accent.clone();
        theme.sequence_activation_border = palette.border.clone();
        let svg =
            mermaid_rs_renderer::render_with_options(source, options).map_err(|e| e.to_string())?;
        if svg.len() > 2 * 1024 * 1024 {
            return Err("Diagram output exceeds preview size limit".into());
        }
        Ok(svg)
    })
    .unwrap_or_else(|_| Err("Diagram could not be rendered".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    const CORPUS: &[(&str, &str)] = &[
        (
            "flowchart",
            include_str!("../../../../scripts/fixtures/markdown-preview/flowchart.mmd"),
        ),
        (
            "sequence",
            include_str!("../../../../scripts/fixtures/markdown-preview/sequence.mmd"),
        ),
        (
            "class",
            include_str!("../../../../scripts/fixtures/markdown-preview/class.mmd"),
        ),
        (
            "state",
            include_str!("../../../../scripts/fixtures/markdown-preview/state.mmd"),
        ),
        (
            "er",
            include_str!("../../../../scripts/fixtures/markdown-preview/er.mmd"),
        ),
        (
            "gantt",
            include_str!("../../../../scripts/fixtures/markdown-preview/gantt.mmd"),
        ),
    ];
    #[test]
    fn corpus_renders_through_gpui_in_both_themes() {
        let renderer = gpui::SvgRenderer::new(std::sync::Arc::new(crate::icons::Assets));
        let mut light_mono = Theme::light();
        light_mono.font_sans = "Geist Mono".into();
        let mut dark_mono = Theme::dark();
        dark_mono.font_sans = "Geist Mono".into();
        for (mode, theme) in [
            ("light", Theme::light()),
            ("dark", Theme::dark()),
            ("light-mono", light_mono),
            ("dark-mono", dark_mono),
        ] {
            let palette = Palette::from_theme(&theme);
            for (name, source) in CORPUS {
                let svg = render(source, &palette).unwrap_or_else(|e| panic!("{mode}/{name}: {e}"));
                assert!(!svg.contains("<foreignObject"));
                let raster = renderer.render_single_frame(svg.as_bytes(), 1.0).unwrap();
                assert!(raster.size(0).width.0 > 0);
                let prepared = crate::files::markdown_media::decode_image(
                    "image/svg+xml",
                    svg.as_bytes().to_vec(),
                )
                .unwrap();
                let prepared_raster = prepared
                    .image
                    .to_image_data(gpui::SvgRenderer::new(std::sync::Arc::new(
                        crate::icons::Assets,
                    )))
                    .unwrap();
                let size = prepared_raster.size(0);
                assert!(size.width.0 > 0 && size.height.0 > 0);
                assert!(size.width.0 <= 4096 && size.height.0 <= 4096);
                assert!(size.width.0 as usize * size.height.0 as usize <= 1024 * 1024);
                let ratio = size.width.0 as f32 / size.height.0 as f32;
                assert!((ratio / (prepared.width / prepared.height) - 1.0).abs() < 0.02);
                assert_eq!(svg, render(source, &palette).unwrap());
                if let Ok(dir) = std::env::var("ZERON_MERMAID_ARTIFACTS") {
                    std::fs::create_dir_all(&dir).unwrap();
                    std::fs::write(format!("{dir}/{name}-{mode}.svg"), svg).unwrap();
                    let size = prepared_raster.size(0);
                    let mut rgba = prepared_raster.as_bytes(0).unwrap().to_vec();
                    for pixel in rgba.chunks_exact_mut(4) {
                        pixel.swap(0, 2);
                    }
                    image::save_buffer(
                        format!("{dir}/{name}-{mode}.png"),
                        &rgba,
                        size.width.0 as u32,
                        size.height.0 as u32,
                        image::ColorType::Rgba8,
                    )
                    .unwrap();
                }
            }
        }
    }
    #[test]
    fn malformed_and_oversized_diagrams_are_recoverable() {
        let palette = Palette::from_theme(&Theme::dark());
        assert!(render("this is not a diagram", &palette).is_err());
        assert!(render(&"x".repeat(MAX_SOURCE_BYTES + 1), &palette).is_err());
        assert!(render("flowchart TD\nA[Hola<br/>mundo] --> B[Fin]", &palette).is_ok());
    }
}
