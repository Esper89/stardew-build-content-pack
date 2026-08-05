use std::{cmp, fs, path::{Path, PathBuf}, time::SystemTime, vec};
use anyhow::{anyhow, bail, Context};
use crate::{cli, ErrorPath, glob, out};

pub struct File<'l> {
    opts: &'l cli::Options,
    modified: Option<SystemTime>,
    toml: String,
}

impl<'l> crate::File<'l> for File<'l> {
    fn load(opts: &'l cli::Options, path: &'l Path) -> anyhow::Result<Self> {
        let (toml, modified) = load_and_preprocess_toml(path)
            .context("loading and preprocessing toml file")?;

        Ok(File { opts, modified, toml })
    }

    fn modified(&self) -> Option<SystemTime> { self.modified }

    fn write(&self, w: out::FileWriter) -> anyhow::Result<()> {
        let de = serde_toml::Deserializer::parse(&self.toml).context("deserializing toml")?;

        let mut buf = Vec::new();

        let res = match self.opts.pretty_print {
            None => serde_transcode::transcode(de, &mut serde_json::Serializer::new(&mut buf)),
            Some(indent) => serde_transcode::transcode(
                de,
                &mut serde_json::Serializer::with_formatter(
                    &mut buf,
                    serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes()),
                )
            ),
        };
        res.context("transcoding toml to json")?;

        buf.push(b'\n');
        w.write(&buf)
    }
}

fn load_and_preprocess_toml(path: &Path) -> anyhow::Result<(String, Option<SystemTime>)> {
    let root = TomlFile::read(path)?;
    let mut toml = String::with_capacity(root.bytes);
    let mut modified = crate::get_file_modified(&root.path);

    let mut files = vec![root];
    'file: while let Some(mut curr) = files.pop() {
        if let Some(path) = curr.includes.next() {
            let file = TomlFile::read(&path)?;
            files.push(curr); curr = file;

            modified = modified.and_then(|old| {
                crate::get_file_modified(&curr.path).map(|new| cmp::max(old, new))
            });

            if files.iter().find(|file| file.id == curr.id).is_some() {
                bail!("include loop in toml files: {}", path.display())
            }

            files.push(curr);
            continue
        }

        for line in &mut curr.lines {
            let mut line = &*line;

            if line.starts_with("###") {
                line = &line[1..];
            } else if let Some(cmd) = line.strip_prefix("##") {
                let (cmd, arg) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
                let arg = arg.trim_start();
                if cmd.is_empty() {
                    return Err(anyhow!(
                        "escape ## at start of line with ### in toml file: {}",
                        curr.path.display(),
                    ).context("preprocessing toml"))
                }

                match cmd {
                    "include" => {
                        let dir = curr.path
                            .parent()
                            .map_or(
                                &*curr.path,
                                |p| if p.as_os_str().is_empty() { Path::new(".") } else { p },
                            );

                        let paths = glob::glob_files_relative(dir, arg).with_context(|| format!(
                            "matching glob '{arg}' in toml file: {}",
                            curr.path.display(),
                        ))?;

                        curr.includes = paths.into_iter();
                        files.push(curr);
                        continue 'file
                    },

                    _ => return Err(anyhow!(
                        "unknown preprocessor command '{cmd}' in toml file: {}",
                        curr.path.display(),
                    )),
                }
            }

            toml.push_str(line);
            toml.push('\n');
        }
    }

    Ok((toml, modified))
}

struct TomlFile {
    path: PathBuf,
    id: file_id::FileId,
    bytes: usize,
    lines: vec::IntoIter<String>,
    includes: vec::IntoIter<PathBuf>,
}

impl TomlFile {
    fn read(path: &Path) -> anyhow::Result<Self> {
        let path = fs::canonicalize(path)
            .err_path(path).context("canonicalizing path to toml file")?;

        let id = file_id::get_file_id(&path)
            .err_path(&path).context("getting id for toml file")?;

        let text = fs::read_to_string(&path)
            .err_path(&path).context("reading toml file")?;

        let bytes = text.len();
        let lines = text.lines().map(str::to_owned).collect::<Vec<_>>().into_iter();
        let includes = vec![].into_iter();

        Ok(TomlFile { path, id, bytes, lines, includes })
    }
}
