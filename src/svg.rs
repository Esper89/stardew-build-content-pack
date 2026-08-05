use std::{cmp, fs, path::{self, Path, PathBuf}, num::NonZero, time::SystemTime};
use anyhow::{anyhow, Context};
use parking_lot::Mutex;
use crate::{cli, ErrorPath, out};

pub struct File<'l> {
    opts: &'l cli::Options,
    modified: Option<SystemTime>,
    tree: resvg::usvg::Tree,
}

impl<'l> crate::File<'l> for File<'l> {
    fn load(opts: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self> {
        let path = fs::canonicalize(path)
            .err_path(path).context("canonicalizing path to svg file")?;

        let modified = Mutex::new(crate::get_file_modified(&path));
        let default_string_resolver = resvg::usvg::ImageHrefResolver::default_string_resolver();
        let resolve_string = Box::new(|href: &str, opts: &resvg::usvg::Options| {
            let res = default_string_resolver(href, opts);
            if res.is_some() {
                let new = opts.resources_dir.as_ref()
                    .and_then(|dir| join_canonical_path(dir, href).ok())
                    .and_then(|path| crate::get_file_modified(&path));

                let mut guard = modified.lock();
                *guard = guard.and_then(|old| new.map(|new| cmp::max(old, new)));
                drop(guard);
            }
            res
        });

        let mut svgopts = resvg::usvg::Options {
            resources_dir: path.parent().map(Path::to_path_buf),
            dpi: 32.0,
            font_size: 8.0,
            default_size: resvg::usvg::Size::from_wh(16.0, 16.0).expect("invalid size"),
            shape_rendering: resvg::usvg::ShapeRendering::CrispEdges,
            text_rendering: resvg::usvg::TextRendering::GeometricPrecision,
            image_rendering: resvg::usvg::ImageRendering::Pixelated,
            image_href_resolver: resvg::usvg::ImageHrefResolver {
                resolve_string,
                ..Default::default()
            },
            ..Default::default()
        };
        svgopts.fontdb_mut().load_system_fonts();

        let svg = fs::read(path).context("reading svg file")?;
        let tree = resvg::usvg::Tree::from_data(&svg, &svgopts).context("parsing svg data")?;

        drop(svgopts);
        let modified = modified.into_inner();

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

fn join_canonical_path(path: &Path, href: &str) -> anyhow::Result<PathBuf> {
    // on windows, canonical paths may not use '/' separators

    let path = if path::MAIN_SEPARATOR == '/' { path.join(href) } else {
        let href = href
            .split('/')
            .flat_map(|seg| [path::MAIN_SEPARATOR_STR, seg])
            .skip(1)
            .collect::<String>();

        path.join(href)
    };

    fs::canonicalize(&path).err_path(&path)
}
