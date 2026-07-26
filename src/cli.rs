use std::{ffi::OsString, path::{Path, PathBuf}, str::FromStr};
use clap::{CommandFactory, Parser};
use anyhow::anyhow;

pub struct Args {
    pub packs: Vec<Pack>,
    pub output: Output,
    pub options: Options,
}

pub struct Pack {
    pub path: PathBuf,
    pub name: OsString,
}

pub struct Output {
    pub dir: Option<PathBuf>,
    pub cache: bool,
    pub zip_file: Option<PathBuf>,
    pub wrap_zip: String,
}

pub struct Options {
    pub pretty_print: Option<&'static str>,
    pub compression: Compression,
    pub features: Features,
}

pub enum Compression { None, Normal, Max }

pub struct Features {
    pub svg: bool,
    pub toml: bool,
}

pub fn args() -> Args {
    let clap = Clap::parse();
    let clap_err = |msg| -> ! {
        Clap::command().error(clap::error::ErrorKind::ValueValidation, msg).exit()
    };

    let mut packs = if clap.named {
        let mut out_packs = Vec::with_capacity(clap.packs.len() / 2);
        let mut in_packs = clap.packs.into_iter();
        while let Some(path) = in_packs.next() {
            let path = PathBuf::from(path);

            let Some(name) = in_packs.next() else {
                clap_err(anyhow!("missing name for path: {}", path.display()))
            };

            out_packs.push(Pack { path, name });
        }
        out_packs
    } else {
        clap.packs.into_iter().map(|path| {
            let path = PathBuf::from(path);
            let name = path.file_name().unwrap_or_default().to_owned();
            Pack { path, name }
        }).collect()
    };

    for Pack { path, name } in &packs {
        if !path.is_dir() {
            clap_err(anyhow!("path is not a usable directory: {}", path.display()))
        }

        if name.is_empty() {
            clap_err(anyhow!("name must not be empty: {}", path.display()))
        }

        if name.as_encoded_bytes().contains(&b'/') || name.as_encoded_bytes().contains(&b'\\') {
            clap_err(anyhow!("name must not contain dir separators: {}", name.display()))
        }
    }

    packs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for [a, b] in packs.array_windows() {
        if a.name == b.name {
            clap_err(anyhow!("duplicate names: {}", a.name.display()))
        }
    }

    if let Some(wrap) = &clap.wrap {
        if wrap.is_empty() {
            clap_err(anyhow!("zip wrap dir must not be empty"))
        }

        if !Path::new(&wrap).is_relative() {
            clap_err(anyhow!("zip wrap dir must be a relative path: {wrap}"))
        }
    }

    Args {
        packs,
        output: Output {
            dir: clap.out,
            cache: !clap.force,
            zip_file: clap.zip,
            wrap_zip: clap.wrap.unwrap_or_default(),
        },
        options: Options {
            pretty_print: clap.print.get(),
            compression: clap.compress.get(),
            features: clap.feature_set.get(),
        },
    }
}

#[derive(Parser)]
#[command(version, about)]
#[group(id = "output", required = true)]
struct Clap {
    /// Content packs to process
    packs: Vec<OsString>,

    /// Each content pack path must be followed by a pack name
    #[arg(short, long)]
    named: bool,

    /// Output directory
    #[arg(short, long, group = "output")]
    out: Option<PathBuf>,

    /// Overwrite output files regardless of timestamp
    #[arg(short, long, requires = "out")]
    force: bool,

    /// Output zip file
    #[arg(short, long, group = "output")]
    zip: Option<PathBuf>,

    /// Wrap zip file output in a directory
    #[arg(short, long, requires = "zip")]
    wrap: Option<String>,

    #[command(flatten)]
    print: Print,

    #[command(flatten)]
    compress: Compress,

    #[command(flatten)]
    feature_set: FeatureSet,
}

#[derive(clap::Args)]
#[group(multiple = false)]
struct Print {
    /// Pretty-print structured output data
    #[arg(short, long, group = "print")]
    pretty: bool,

    /// Pretty-print structured output data with custom indentation
    #[arg(long, group = "print")]
    indent: Option<Indent>,
}

impl Print {
    fn get(self) -> Option<&'static str> {
        match (self.pretty, self.indent) {
            (false, None) => None,
            (true, None) => Some(Indent::default().get()),
            (false, Some(indent)) => Some(indent.get()),
            (true, Some(_)) => unreachable!(),
        }
    }
}

#[derive(Clone)]
enum Indent { Spaces(usize), Tabs }

impl Indent {
    fn get(self) -> &'static str {
        match self {
            Indent::Tabs => "\t",
            Indent::Spaces(n @ 0..=32) => &"                                "[0..n],
            Indent::Spaces(33..) => unreachable!(),
        }
    }
}

impl Default for Indent {
    fn default() -> Self { Indent::Spaces(4) }
}

impl FromStr for Indent {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("tab") || s.eq_ignore_ascii_case("tabs") {
            Ok(Indent::Tabs)
        } else if s.eq_ignore_ascii_case("none") || s.eq_ignore_ascii_case("no") {
            Ok(Indent::Spaces(0))
        } else {
            let n = usize::from_str(s)?;
            if matches!(n, 0..=32) {
                Ok(Indent::Spaces(n))
            } else {
                Err(anyhow!("custom indent may not be greater than 32 spaces"))
            }
        }
    }
}

#[derive(clap::Args)]
#[group(multiple = false)]
struct Compress {
    /// Disable compression of output data where applicable
    #[arg(short = '0', long)]
    no_compress: bool,

    /// Compress output data as much as possible where applicable
    #[arg(short = 'x', long)]
    max_compress: bool,
}

impl Compress {
    fn get(self) -> Compression {
        match (self.no_compress, self.max_compress) {
            (true, false) => Compression::None,
            (false, false) => Compression::Normal,
            (false, true) => Compression::Max,
            (true, true) => unreachable!(),
        }
    }
}

#[derive(clap::Args)]
#[group(multiple = false)]
struct FeatureSet {
    /// Enable only the specified features
    #[arg(short, long, value_delimiter = ',')]
    enable: Option<Vec<Feature>>,

    /// Disable the specified features
    #[arg(short, long, value_delimiter = ',')]
    disable: Option<Vec<Feature>>,
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, clap::ValueEnum)]
enum Feature {
    Svg,
    Toml,
}

impl FeatureSet {
    fn get(mut self) -> Features {
        if let Some(v) = &mut self.enable { v.sort_unstable(); v.dedup() }
        if let Some(v) = &mut self.disable { v.sort_unstable(); v.dedup() }

        match (self.enable, self.disable) {
            (None, None) => Features {
                svg: true,
                toml: true,
            },
            (Some(enable), None) => Features {
                svg: enable.binary_search(&Feature::Svg).is_ok(),
                toml: enable.binary_search(&Feature::Toml).is_ok(),
            },
            (None, Some(disable)) => Features {
                svg: disable.binary_search(&Feature::Svg).is_err(),
                toml: disable.binary_search(&Feature::Toml).is_err(),
            },
            (Some(_), Some(_)) => unreachable!(),
        }
    }
}
