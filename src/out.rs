use std::{
    cmp,
    collections::{HashSet, VecDeque},
    ffi::{OsStr, OsString},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};
use anyhow::{anyhow, bail, Context};
use crate::{cli, ErrorPath, FileType};

pub struct Output {
    dir: Option<OutDir>,
    zip: Option<OutZip>,
    packs: HashSet<OsString>,
}

struct OutDir {
    path: PathBuf,
    cache: bool,
}

struct OutZip {
    writer: ZipWriter,
    prefix: String,
    dir_opts: zip::write::FileOptions<'static, ()>,
    file_opts: zip::write::FileOptions<'static, ()>,
}

type ZipWriter = zip::ZipWriter<io::BufWriter<fs::File>>;

impl Output {
    pub fn setup(output: cli::Output, opts: &cli::Options) -> anyhow::Result<Self> {
        let dir = if let Some(path) = output.dir {
            fs::create_dir_all(&path).err_path(&path).context("creating output dir")?;
            Some(OutDir { path, cache: output.cache })
        } else { None };

        let zip = if let Some(path) = output.zip_file {
            let prefix = normalize_relative_path(&output.wrap_zip);

            let file = fs::File::create(&path).err_path(&path).context("creating zip archive")?;
            let mut writer = zip::ZipWriter::new(io::BufWriter::new(file));

            let file_opts = zip::write::FileOptions::default()
                .system(zip::System::Dos)
                .unix_permissions(0o664)
                .compression_method(match opts.compression {
                    cli::Compression::None => zip::CompressionMethod::Stored,
                    cli::Compression::Normal | cli::Compression::Max
                        => zip::CompressionMethod::Deflated,
                })
                .compression_level(match opts.compression {
                    cli::Compression::None => None,
                    cli::Compression::Normal => Some(9),
                    cli::Compression::Max => Some(73),
                })
                .with_zopfli_buffer(match opts.compression {
                    cli::Compression::None | cli::Compression::Normal => None,
                    cli::Compression::Max => Some(1 << 16),
                });
            let dir_opts = file_opts.unix_permissions(0o775);

            let mut dir = String::with_capacity(prefix.len());
            for name in prefix.split_terminator('/') {
                dir.push_str(name);
                writer.add_directory(&dir, dir_opts).context("creating wrap dir in zip archive")?;
                dir.push('/');
            }

            let mut zip = OutZip { writer, prefix, dir_opts, file_opts };
            zip.writer.flush().context("writing zip archive")?;
            Some(zip)
        } else { None };

        Ok(Output { dir, zip, packs: HashSet::new() })
    }

    pub fn pack(&mut self, name: &OsStr) -> anyhow::Result<SubFolder> {
        if !self.packs.insert(name.to_owned()) {
            bail!("duplicate pack names: {}", name.display())
        }

        let dir = if let Some(dir) = &self.dir {
            let path = dir.path.join(name);
            match FileType::if_exists(&path).context("checking old pack dir")? {
                None => fs::create_dir_all(&path).err_path(&path).context("creating pack dir")?,
                Some(FileType::Dir) => (),
                Some(FileType::File) => fs::remove_file(&path).err_path(&path)
                    .context("removing old file")?,
            }
            Some(SubOutDir { path })
        } else { None };

        let zip = if let Some(zip) = &mut self.zip {
            let name = name.to_str()
                .ok_or_else(|| anyhow!("pack name is not valid unicode"))
                .err_path(name)
                .context("adding pack to zip archive")?;

            let mut prefix = String::with_capacity(zip.prefix.len() + name.len() + 1);
            prefix.clone_from(&zip.prefix);
            prefix.push_str(name);
            zip.writer.add_directory(&prefix, zip.dir_opts).context("creating dir in zip archive")?;
            prefix.push('/');

            zip.writer.flush().context("writing zip archive")?;
            Some(SubOutZip { prefix })
        } else { None };

        Ok(SubFolder { dir, zip, entries: HashSet::new() })
    }

    pub fn finish(self) -> anyhow::Result<()> {
        if let Some(dir) = self.dir {
            clean_dir(&dir.path, |entry| self.packs.contains(entry))
                .context("cleaning output dir")?;
        }

        if let Some(mut zip) = self.zip {
            zip.writer.flush().context("writing zip archive")?;
            zip.writer.finish()
                .map_err(anyhow::Error::new)
                .and_then(|mut f| f.flush().map_err(Into::into))
                .context("closing zip archive")?;
        }

        Ok(())
    }
}

pub struct SubFolder {
    dir: Option<SubOutDir>,
    zip: Option<SubOutZip>,
    entries: HashSet<OsString>,
}

struct SubOutDir {
    path: PathBuf,
}

struct SubOutZip {
    prefix: String,
}

impl SubFolder {
    pub fn dir(&mut self, out: &mut Output, name: &OsStr) -> anyhow::Result<SubFolder> {
        if !self.entries.insert(name.to_owned()) {
            bail!("duplicate dir entries: {}", name.display())
        }

        let dir = if let Some(dir) = &self.dir {
            let path = dir.path.join(name);
            match FileType::if_exists(&path).context("checking old output dir")? {
                None => fs::create_dir_all(&path).err_path(&path).context("creating subdir")?,
                Some(FileType::Dir) => (),
                Some(FileType::File) => fs::remove_file(&path).err_path(&path)
                    .context("removing old output file")?,
            }
            Some(SubOutDir { path })
        } else { None };

        let zip = if let (Some(zip), Some(sub_zip)) = (&mut out.zip, &mut self.zip) {
            let name = name.to_str()
                .ok_or_else(|| anyhow!("dir name is not valid unicode"))
                .err_path(name)
                .context("adding subdir to zip archive")?;

            let mut prefix = String::with_capacity(sub_zip.prefix.len() + name.len() + 1);
            prefix.clone_from(&sub_zip.prefix);
            prefix.push_str(name);
            zip.writer.add_directory(&prefix, zip.dir_opts).context("creating dir in zip archive")?;
            prefix.push('/');

            zip.writer.flush().context("writing zip archive")?;
            Some(SubOutZip { prefix })
        } else { None };

        Ok(SubFolder { dir, zip, entries: HashSet::new() })
    }

    pub fn file(
        &mut self, out: &mut Output, name: &OsStr, src: &Path,
        f: impl FnOnce(FileWriter) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if !self.entries.insert(name.to_owned()) {
            bail!("duplicate dir entries: {}", name.display())
        }

        let mut skip = true;

        let file_path = if let (Some(dir), Some(sub_dir)) = (&mut out.dir, &mut self.dir) {
            let path = sub_dir.path.join(name);
            if dir.cache && cmp_files_modified(&path, src) == Some(cmp::Ordering::Less) { None }
            else {
                match FileType::if_exists(&path).context("checking old output file")? {
                    None | Some(FileType::File) => (),
                    Some(FileType::Dir) => fs::remove_dir_all(&path).err_path(&path)
                        .context("removing old output dir")?,
                }
                skip = false;
                Some(path)
            }
        } else { None };
        let file_path = file_path.as_deref();

        let zip_writer = if let (Some(zip), Some(sub_zip)) = (&mut out.zip, &mut self.zip) {
            let name = name.to_str()
                .ok_or_else(|| anyhow!("file name is not valid unicode"))
                .err_path(name)
                .context("adding file to zip archive")?;

            let mut path = String::with_capacity(sub_zip.prefix.len() + name.len());
            path.clone_from(&sub_zip.prefix);
            path.push_str(name);
            zip.writer.start_file(&path, zip.file_opts).context("creating file in zip archive")?;

            skip = false;
            Some(&mut zip.writer)
        } else { None };

        if !skip {
            let w = FileWriter { file_path, zip_writer, src };
            f(w)?;
        }

        if let Some(zip) = &mut out.zip {
            zip.writer.flush().context("writing zip archive")?;
        }

        Ok(())
    }

    pub fn finish(self) -> anyhow::Result<()> {
        if let Some(sub_dir) = &self.dir {
            clean_dir(&sub_dir.path, |entry| self.entries.contains(entry))
                .context("cleaning subdir")?;
        }

        Ok(())
    }
}

pub struct FileWriter<'w> {
    file_path: Option<&'w Path>,
    zip_writer: Option<&'w mut ZipWriter>,
    src: &'w Path,
}

impl FileWriter<'_> {
    pub fn write(self, bytes: &[u8]) -> anyhow::Result<()> {
        if let Some(path) = self.file_path {
            fs::write(path, bytes).err_path(path).context("writing bytes to output file")?;
        }

        if let Some(zip) = self.zip_writer {
            zip.write_all(bytes).context("writing file bytes to zip archive")?;
        }

        Ok(())
    }

    pub fn copy(self) -> anyhow::Result<()> {
        if let Some(path) = self.file_path {
            fs::copy(self.src, path).with_context(|| format!(
                "copying file {} to {}", self.src.display(), path.display(),
            ))?;
        }

        if let Some(zip) = self.zip_writer {
            let mut file = fs::File::open(self.src).err_path(self.src).with_context(|| format!(
                "opening {} to copy into zip archive", self.src.display(),
            ))?;

            io::copy(&mut file, zip).with_context( || format!(
                "copying {} into zip archive", self.src.display(),
            ))?;
        }

        Ok(())
    }
}

fn clean_dir(dir: &Path, keep: impl Fn(&OsStr) -> bool) -> anyhow::Result<()> {
    let mut rm = Vec::new();
    for entry in fs::read_dir(dir).context("reading dir").err_path(dir)? {
        let name = entry.context("reading dir").err_path(dir)?.file_name();
        if !keep(&name) { rm.push(name) }
    }

    if !rm.is_empty() {
        let mut path = dir.to_owned();
        for name in rm {
            path.push(name);
            match FileType::for_path(&path).context("checking file for removal")? {
                FileType::Dir => fs::remove_dir_all(&path)
                    .context("removing dir").err_path(&path)?,

                FileType::File => fs::remove_file(&path)
                    .context("removing file").err_path(&path)?,
            }
            path.pop();
        }
    }

    Ok(())
}

fn cmp_files_modified(a: &Path, b: &Path) -> Option<cmp::Ordering> {
    let get_file_modified = |path| fs::metadata(path).ok()
        .and_then(|meta| if meta.is_file() { meta.modified().ok() } else { None });

    let a_modified = get_file_modified(a)?;
    let b_modified = get_file_modified(b)?;
    Some(a_modified.cmp(&b_modified))
}

fn normalize_relative_path(path: &str) -> String {
    let mut segments = path
        .split(['/', '\\'])
        .filter(|s| !s.is_empty() && *s != ".")
        .collect::<VecDeque<_>>();

    let mut i = 0;
    while i < segments.len() {
        if segments[i] == ".." {
            segments.remove(i);
            if let Some(new_i) = i.checked_sub(1) {
                i = new_i;
                segments.remove(i);
            }
        }

        i += 1;
    }

    segments.into_iter().flat_map(|s| [s, "/"]).collect()
}
