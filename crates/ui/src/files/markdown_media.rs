//! Bounded media decoding. SVG resources are resolved in memory, never on the UI host.
use crate::markdown::parser::{Block, BlockTree, InlineRun};
use gpui::{Image, ImageFormat};
use std::{
    io::Cursor,
    sync::{Arc, OnceLock},
};

#[derive(Clone)]
pub(super) struct MediaImage {
    pub image: Arc<Image>,
    pub width: f32,
    pub height: f32,
    pub bytes: usize,
}

pub(super) fn image_sources(tree: &BlockTree) -> Vec<String> {
    fn runs(runs: &[InlineRun], out: &mut Vec<String>) {
        for run in runs {
            if let Some(image) = &run.style.image {
                if !out.contains(&image.source) {
                    out.push(image.source.clone());
                }
            }
        }
    }
    fn block(b: &Block, out: &mut Vec<String>) {
        match b {
            Block::Paragraph { runs: r } | Block::Heading { runs: r, .. } => runs(r, out),
            Block::BlockQuote { children } => children.iter().for_each(|b| block(b, out)),
            Block::List { items, .. } => items.iter().flatten().for_each(|b| block(b, out)),
            Block::Table { header, rows, .. } => header
                .iter()
                .chain(rows.iter().flatten())
                .for_each(|r| runs(r, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for top in &tree.blocks {
        block(&top.block, &mut out);
    }
    out
}

pub(super) fn diagram_sources(tree: &BlockTree) -> Vec<String> {
    fn visit(block: &Block, out: &mut Vec<String>) {
        match block {
            Block::CodeBlock { language, code }
                if language
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case("mermaid")) =>
            {
                if !out.contains(code) {
                    out.push(code.clone());
                }
            }
            Block::BlockQuote { children } => children.iter().for_each(|b| visit(b, out)),
            Block::List { items, .. } => items.iter().flatten().for_each(|b| visit(b, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for top in &tree.blocks {
        visit(&top.block, &mut out);
    }
    out
}

pub(super) fn svg_options() -> usvg::Options<'static> {
    static FONTS: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fonts = FONTS
        .get_or_init(|| {
            let mut db = usvg::fontdb::Database::new();
            db.load_system_fonts();
            Arc::new(db)
        })
        .clone();
    usvg::Options {
        fontdb: fonts,
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_data: Box::new(|_, _, _| None),
            resolve_string: Box::new(|_, _| None),
        },
        ..Default::default()
    }
}

pub(super) fn decode_image(mime: &str, bytes: Vec<u8>) -> Result<MediaImage, String> {
    if bytes.len() > zeron_proto::MAX_WORKSPACE_IMAGE_BYTES {
        return Err("Image exceeds preview size limit".into());
    }
    if mime == "image/svg+xml" {
        let tree = usvg::Tree::from_data(&bytes, &svg_options()).map_err(|e| e.to_string())?;
        let (width, height) = (tree.size().width(), tree.size().height());
        if width > 2048.0 || height > 2048.0 {
            return Err("SVG exceeds 2048px preview limit".into());
        }
        // Re-serialize the parsed tree: scripts, HTML and external resources never reach GPUI.
        let svg = tree.to_string(&usvg::WriteOptions::default());
        let retained = svg.len() + (width * height * 16.0) as usize;
        return Ok(MediaImage {
            image: Arc::new(Image::from_bytes(ImageFormat::Svg, svg.into_bytes())),
            width,
            height,
            bytes: retained,
        });
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    // Decode one frame and encode a static PNG so GPUI cannot expand unbounded animation frames.
    let decoded = reader.decode().map_err(|e| e.to_string())?;
    let (width, height) = (decoded.width() as f32, decoded.height() as f32);
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let bytes = png.into_inner();
    let retained = bytes.len() + (width * height * 4.0) as usize;
    Ok(MediaImage {
        image: Arc::new(Image::from_bytes(ImageFormat::Png, bytes)),
        width,
        height,
        bytes: retained,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_collection_preserves_nested_and_repeated_media() {
        let tree = crate::markdown::parser::parse_full(
            "before ![a](a.png) after\n\n> ![b](b.svg)\n\n- ![a](a.png)",
        );
        assert_eq!(image_sources(&tree), ["a.png", "b.svg"]);
    }
    #[test]
    fn svg_is_bounded_and_external_resources_are_removed() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="100"><image href="file:///etc/passwd" width="20" height="20"/><rect width="10" height="10"/></svg>"##;
        assert!(decode_image("image/svg+xml", svg.to_vec()).is_ok());
        assert!(
            decode_image(
                "image/svg+xml",
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="99999" height="10"/>"#.to_vec()
            )
            .is_err()
        );
        assert!(decode_image("image/png", b"not an image".to_vec()).is_err());
    }
}
