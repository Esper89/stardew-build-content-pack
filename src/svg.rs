use std::{fs, path::Path, num::NonZero, time::SystemTime};
use anyhow::{anyhow, Context};
use crate::{cli, out};

pub struct File<'l> {
    opts: &'l cli::Options,
    modified: Option<SystemTime>,
    tree: resvg::usvg::Tree,
}

impl<'l> crate::File<'l> for File<'l> {
    fn load(opts: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self> {
        let modified = crate::get_file_modified(path);

        let mut svgopts = resvg::usvg::Options {
            resources_dir: fs::canonicalize(path)
                .ok().and_then(|p| p.parent().map(Path::to_path_buf)),
            dpi: 32.0,
            font_size: 8.0,
            default_size: resvg::usvg::Size::from_wh(16.0, 16.0).expect("invalid size"),
            shape_rendering: resvg::usvg::ShapeRendering::CrispEdges,
            text_rendering: resvg::usvg::TextRendering::GeometricPrecision,
            image_rendering: resvg::usvg::ImageRendering::Pixelated,
            ..Default::default()
        };
        svgopts.fontdb_mut().load_system_fonts();

        let svg = fs::read(path).context("reading svg file")?;
        let tree = resvg::usvg::Tree::from_data(&svg, &svgopts).context("parsing svg data")?;
        Ok(File { opts, modified, tree })
    }

    fn modified(&self) -> Option<SystemTime> { self.modified }

    fn write(&self, w: out::FileWriter) -> anyhow::Result<()> {
        let size = self.tree.size().to_int_size();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
            .ok_or_else(|| anyhow!("svg has invalid size"))?;
        resvg::render(&self.tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());

        let (width, height, rgba) = (pixmap.width(), pixmap.height(), pixmap.take_demultiplied());
        let mut png = Vec::new();
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::NoCompression);
        encoder.set_filter(png::Filter::NoFilter);
        let mut writer = encoder.write_header().context("writing png header")?;
        writer.write_image_data(&rgba).context("writing png data")?;
        writer.finish().context("writing png data")?;

        let oxiopts = match self.opts.compression {
            cli::Compression::None => None,
            cli::Compression::Normal => Some(oxipng::Options {
                optimize_alpha: true,
                strip: oxipng::StripChunks::All,
                ..oxipng::Options::from_preset(4)
            }),
            cli::Compression::Max => Some(oxipng::Options {
                optimize_alpha: true,
                strip: oxipng::StripChunks::All,
                deflater: oxipng::Deflater::Zopfli(oxipng::ZopfliOptions {
                    iteration_count: const { NonZero::new(64).unwrap() },
                    iterations_without_improvement: const { NonZero::new(8).unwrap() },
                    maximum_block_splits: 16,
                }),
                ..oxipng::Options::max_compression()
            }),
        };

        if let Some(oxiopts) = &oxiopts {
            png = oxipng::optimize_from_memory(&png, oxiopts).context("optimizing png data")?;
        }

        w.write(&png)
    }
}
