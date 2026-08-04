use std::{
    cmp,
    fmt,
    fs,
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    process,
    sync::atomic,
    time::SystemTime,
    vec,
};
use anyhow::{anyhow, bail, Context};

mod cli;
mod glob;
mod out;
mod svg;
mod toml;

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

                FileType::File => {
                    let src = SourceFile::load(opts, &path)
                        .with_context(|| format!("loading file: {}", path.display()))?;

                    curr.out.file(out, &name, &src)?;
                },
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
                    FileType::File => map_file_ext(opts, entry.file_name()),
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

fn map_file_ext(opts: &cli::Options, name: OsString) -> OsString {
    let mut path = PathBuf::from(name);

    let ext = match path.extension().and_then(OsStr::to_str).unwrap_or("") {
        ext if opts.features.svg && ext.eq_ignore_ascii_case("svg") => Some("png"),
        ext if opts.features.toml && ext.eq_ignore_ascii_case("toml") => Some("json"),
        _ => None,
    };

    if let Some(ext) = ext { path.set_extension(ext); }
    path.into_os_string()
}

trait File<'l>: Sized {
    fn load(opts: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self>;
    fn modified(&self) -> Option<SystemTime>;
    fn write(&self, w: out::FileWriter) -> anyhow::Result<()>;
}

enum SourceFile<'l> {
    Svg(svg::File<'l>),
    Toml(toml::File<'l>),
    Other(OtherFile<'l>),
}

impl<'l> File<'l> for SourceFile<'l> {
    fn load(opts: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self> {
        match path.extension().and_then(OsStr::to_str).unwrap_or("") {
            ext if opts.features.svg && ext.eq_ignore_ascii_case("svg")
                => svg::File::load(opts, path).map(SourceFile::Svg),

            ext if opts.features.toml && ext.eq_ignore_ascii_case("toml")
                => toml::File::load(opts, path).map(SourceFile::Toml),

            _ => OtherFile::load(opts, path).map(SourceFile::Other),
        }
    }

    fn modified(&self) -> Option<SystemTime> {
        match self {
            SourceFile::Svg(file) => file.modified(),
            SourceFile::Toml(file) => file.modified(),
            SourceFile::Other(file) => file.modified(),
        }
    }

    fn write(&self, w: out::FileWriter) -> anyhow::Result<()> {
        match self {
            SourceFile::Svg(file) => file.write(w),
            SourceFile::Toml(file) => file.write(w),
            SourceFile::Other(file) => file.write(w),
        }
    }
}

struct OtherFile<'l> {
    path: &'l Path,
    modified: Option<SystemTime>,
}

impl<'l> File<'l> for OtherFile<'l> {
    fn load(_: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self> {
        Ok(OtherFile { path, modified: get_file_modified(path) })
    }

    fn modified(&self) -> Option<SystemTime> { self.modified }
    fn write(&self, w: out::FileWriter) -> anyhow::Result<()> { w.copy(self.path) }
}

fn get_file_modified(path: &Path) -> Option<SystemTime> {
    let meta = fs::symlink_metadata(path).ok()?;
    if meta.is_file() { return meta.modified().ok() }

    let mut modified = meta.modified().ok()?;
    let mut path = fs::read_link(path).ok()?;
    loop {
        let meta = fs::symlink_metadata(&path).ok()?;
        modified = cmp::max(modified, meta.modified().ok()?);
        if meta.is_file() { return Some(modified) }

        path = fs::read_link(&path).ok()?;
    }
}
