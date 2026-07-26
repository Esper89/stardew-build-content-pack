use std::{
    fmt,
    fs,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    num::NonZero,
    process,
    sync::atomic,
    vec,
};
use anyhow::{anyhow, bail, Context};

mod cli;
mod out;

fn main() -> process::ExitCode {
    handle_err(run(cli::args()));
    if get_err() { process::ExitCode::FAILURE } else { process::ExitCode::SUCCESS }
}

fn run(args: cli::Args) -> anyhow::Result<()> {
    let mut out = out::Output::setup(args.output, &args.options).context("setting up output")?;
    for pack in args.packs {
        handle_err(build_pack(&pack, &args.options, &mut out).context("building pack"));
    }
    out.finish().context("cleaning up output")
}

fn build_pack(pack: &cli::Pack, opts: &cli::Options, out: &mut out::Output) -> anyhow::Result<()> {
    let pack = BuildDir::new(opts, &pack.path, || out.pack(&pack.name))
        .context("setting up pack")?;
    let Some(pack) = pack else { return Ok(()) };

    let mut dirs = vec![pack];
    while let Some(mut curr) = dirs.pop() {
        while let Some(DirEntry { path, ty, name }) = curr.entries.next() {
            match ty {
                FileType::Dir => {
                    let dir = BuildDir::new(opts, &path, || curr.out.dir(out, &name))
                        .context("setting up dir")?;

                    if let Some(dir) = dir {
                        dirs.push(curr); curr = dir;

                        if dirs.iter().find(|dir| dir.id == curr.id).is_some() {
                            bail!("symlink loop in input dirs: {}", path.display())
                        }
                    }
                },

                FileType::File => curr.out.file(out, &name, &path, |w| {
                    let ext = path.extension().and_then(OsStr::to_str);
                    process_file(opts, &path, ext, w)
                        .with_context(|| format!("processing file: {}", path.display()))
                })?,
            }
        }

        curr.out.finish().context("cleaning up dir")?;
    }

    Ok(())
}

struct BuildDir {
    id: file_id::FileId,
    out: out::SubFolder,
    entries: vec::IntoIter<DirEntry>,
}

struct DirEntry {
    path: PathBuf,
    ty: FileType,
    name: OsString,
}

impl BuildDir {
    fn new(
        opts: &cli::Options,
        path: &Path,
        out: impl FnOnce() -> anyhow::Result<out::SubFolder>,
    ) -> anyhow::Result<Option<Self>> {
        let mut entries = fs::read_dir(path)
            .err_path(path)
            .context("reading input dir")?
            .map(|entry| {
                let entry = entry.err_path(path)?;
                let path = entry.path();
                let ty = FileType::for_path(&path)?;
                let name = match ty {
                    FileType::Dir => entry.file_name(),
                    FileType::File => {
                        let mut name = PathBuf::from(entry.file_name());
                        let set_ext = name
                            .extension()
                            .and_then(OsStr::to_str)
                            .map(str::to_ascii_lowercase)
                            .and_then(|ext| map_file_ext(opts, &ext));

                        if let Some(ext) = set_ext { name.set_extension(ext); }
                        name.into_os_string()
                    },
                };

                Ok(DirEntry { path, ty, name })
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .context("reading input dir")?;

        entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        for [a, b] in entries.array_windows() {
            if a.name == b.name {
                bail!("duplicate names {} in dir: {}", a.name.display(), path.display())
            }
        }

        let skip = entries
            .iter()
            .find(|entry| entry.name.eq_ignore_ascii_case(".content-pack-skip"))
            .is_some();

        if skip { Ok(None) } else {
            Ok(Some(BuildDir {
                id: file_id::get_file_id(path)
                    .err_path(path).context("getting file id for input dir")?,
                out: out()?,
                entries: entries.into_iter(),
            }))
        }
    }
}

static ERROR: atomic::AtomicBool = atomic::AtomicBool::new(false);
fn get_err() -> bool { ERROR.load(atomic::Ordering::Relaxed) }
fn set_err() { ERROR.store(true, atomic::Ordering::Relaxed); }

fn handle_err<T>(res: anyhow::Result<T>) -> Option<T> {
    match res {
        Ok(res) => Some(res),
        Err(e) => { eprintln!("Error: {e:?}"); set_err(); None },
    }
}

trait ErrorPath {
    type Output;
    fn err_path(self, path: &(impl AsRef<Path> + ?Sized)) -> Self::Output;
}

impl<T, E> ErrorPath for Result<T, E> where E: fmt::Display {
    type Output = anyhow::Result<T>;
    fn err_path(self, path: &(impl AsRef<Path> + ?Sized)) -> anyhow::Result<T> {
        let path = path.as_ref();
        self.map_err(|err| anyhow!("{err}: {}", path.display()))
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum FileType { Dir, File }

impl FileType {
    fn if_exists(path: &Path) -> anyhow::Result<Option<Self>> {
        match fs::metadata(path) {
            Ok(meta) => FileType::for_meta(&meta).err_path(path).map(Some),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn for_path(path: &Path) -> anyhow::Result<Self> {
        fs::metadata(path)
            .map_err(Into::into)
            .and_then(|meta| FileType::for_meta(&meta))
            .err_path(path)
    }

    fn for_meta(meta: &fs::Metadata) -> anyhow::Result<Self> {
        if meta.is_dir() { Ok(FileType::Dir) }
        else if meta.is_file() { Ok(FileType::File) }
        else { Err(anyhow!("mysterious file type")) }
    }
}

fn map_file_ext(opts: &cli::Options, ext: &str) -> Option<&'static str> {
    match ext {
        "svg" if opts.features.svg => Some("png"),
        "toml" if opts.features.toml => Some("json"),
        _ => None,
    }
}

fn process_file(
    opts: &cli::Options,
    path: &Path,
    ext: Option<&str>,
    w: out::FileWriter,
) -> anyhow::Result<()> {
    match ext {
        Some("svg") if opts.features.svg => w.write(&svg_to_png(opts, path)?),
        Some("toml") if opts.features.toml => w.write(&toml_to_json(opts, path)?),
        _ => w.copy(),
    }
}

fn svg_to_png(opts: &cli::Options, path: &Path) -> anyhow::Result<Vec<u8>> {
    let tree = {
        let mut opt = resvg::usvg::Options {
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
        opt.fontdb_mut().load_system_fonts();

        let svg = fs::read(path).context("reading svg file")?;
        resvg::usvg::Tree::from_data(&svg, &opt).context("parsing svg data")?
    };

    let size = tree.size().to_int_size();
    let mut pixmap = resvg::tiny_skia::Pixmap::new(size.width(), size.height())
        .ok_or_else(|| anyhow!("svg has invalid size"))?;
    resvg::render(&tree, resvg::tiny_skia::Transform::default(), &mut pixmap.as_mut());

    let (w, h, rgba) = (pixmap.width(), pixmap.height(), pixmap.take_demultiplied());
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, w, h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::NoCompression);
    encoder.set_filter(png::Filter::NoFilter);
    let mut writer = encoder.write_header().context("writing png header")?;
    writer.write_image_data(&rgba).context("writing png data")?;
    writer.finish().context("writing png data")?;

    let oxiopts = match opts.compression {
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
        oxipng::optimize_from_memory(&png, oxiopts).context("optimizing png data")
    } else { Ok(png) }
}

fn toml_to_json(opts: &cli::Options, path: &Path) -> anyhow::Result<Vec<u8>> {
    let toml = fs::read_to_string(path).err_path(path).context("reading toml file")?;
    let de = toml::Deserializer::parse(&toml).context("deserializing toml")?;

    let mut buf = Vec::new();

    let res = match opts.pretty_print {
        None => serde_transcode::transcode(de, &mut serde_json::Serializer::new(&mut buf)),
        Some(indent) => serde_transcode::transcode(de, &mut serde_json::Serializer::with_formatter(
            &mut buf,
            serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes()),
        )),
    };
    res.context("transcoding toml to json")?;

    buf.push(b'\n');
    Ok(buf)
}
