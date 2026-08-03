use std::{ffi::OsString, fs, iter, num::NonZero, ops, path::{Path, PathBuf}, str, vec};
use anyhow::{anyhow, bail, Context};
use bitvec::boxed::BitBox;
use caseless::Caseless;
use unicode_normalization::UnicodeNormalization;
use unicode_segmentation::{UnicodeSegmentation, Graphemes};
use crate::{ErrorPath, FileType};

// existing globbing crates don't support unicode case insensitivity or path traversal
// this implementation is not as fast or featureful but supports what is needed

pub fn glob_files_relative(dir: &Path, glob: &str) -> anyhow::Result<Vec<PathBuf>> {
    let glob = parse(glob).context("parsing glob")?;
    if glob.trailing_sep { bail!("glob with trailing separator will not match any files") }
    if glob.base_path.segments.is_empty() && glob.floating_paths.is_empty() {
        bail!("glob with zero path segments will not match any files")
    }

    let mut dir = dir.to_owned();
    for _ in 0..glob.parent_segs { dir.pop(); }

    let mut matches = vec![];

    if glob.base_path.segments.is_empty() {
        matches.push(dir);
    } else {
        struct BaseMatcher { dir: PathBuf, seg: usize }

        let mut matchers = vec![BaseMatcher { dir, seg: 0 }];

        let last_seg = glob.base_path.segments.len() - 1;
        let match_files = glob.floating_paths.is_empty();

        while let Some(BaseMatcher { dir, seg }) = matchers.pop() {
            let segment = &glob.base_path.segments[seg];

            for entry in fs::read_dir(&dir).err_path(&dir).context("traversing tree")? {
                let name = entry.err_path(&dir).context("traversing tree")?.file_name();

                if segment.match_text(name.as_encoded_bytes()) {
                    let path = dir.join(name);
                    match FileType::for_path(&path).context("traversing tree")? {
                        FileType::File => if seg == last_seg && match_files { matches.push(path) },
                        FileType::Dir => {
                            if seg < last_seg {
                                matchers.push(BaseMatcher { dir: path, seg: seg + 1 });
                            } else if !match_files { matches.push(path) }
                        },
                    }
                }
            }
        }
    }

    if !glob.floating_paths.is_empty() {
        struct FloatingMatcher {
            dir: PathBuf,
            id: file_id::FileId,
            path_idx: usize,
            seg_bits: BitBox,
            entries: vec::IntoIter<OsString>,
        }

        impl FloatingMatcher {
            fn new(dir: PathBuf, path_idx: usize, seg_bits: BitBox) -> anyhow::Result<Self> {
                let id = file_id::get_file_id(&dir).err_path(&dir).context("getting file id")?;
                let entries = fs::read_dir(&dir)
                    .err_path(&dir).context("traversing tree")?
                    .map(|entry| Ok(entry?.file_name()))
                    .collect::<anyhow::Result<Vec<_>>>().context("traversing tree")?
                    .into_iter();

                Ok(FloatingMatcher { dir, id, path_idx, seg_bits, entries })
            }
        }

        let zeroed_seg_bits = |path_idx: usize| {
            bitvec::bitbox![0; glob.floating_paths[path_idx].segments.len().saturating_sub(1)]
        };

        let mut roots = matches
            .into_iter()
            .map(|dir| FloatingMatcher::new(dir, 0, zeroed_seg_bits(0)))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut matchers = vec![];
        matches = vec![];

        if let Some(first) = roots.pop() { matchers.push(first) }
        while let Some(mut curr) = matchers.pop() {
            while let Some(name) = curr.entries.next() {
                let path = curr.dir.join(&name);
                match FileType::for_path(&path).context("traversing tree")? {
                    FileType::File => {
                        if curr.path_idx == glob.floating_paths.len() - 1
                            && curr.seg_bits.last().as_deref().copied().unwrap_or(true)
                            && glob.floating_paths[curr.path_idx].segments.last()
                                .map(|segment| segment.match_text(name.as_encoded_bytes()))
                                .unwrap_or(false)
                        { matches.push(path) }
                    },

                    FileType::Dir => {
                        let mut path_idx = curr.path_idx;
                        let mut seg_bits = zeroed_seg_bits(curr.path_idx);

                        let segs_to_check = iter::once(0)
                            .chain(curr.seg_bits.iter_ones().map(|i| i + 1));

                        for seg_idx in segs_to_check {
                            let segment = &glob.floating_paths[curr.path_idx].segments[seg_idx];
                            if segment.match_text(name.as_encoded_bytes()) {
                                if seg_idx < seg_bits.len() {
                                    seg_bits.set(seg_idx, true);
                                } else if curr.path_idx + 1 < glob.floating_paths.len() {
                                    path_idx = curr.path_idx + 1;
                                    seg_bits = zeroed_seg_bits(path_idx);
                                    break
                                }
                            }
                        }

                        let matcher = FloatingMatcher::new(path, path_idx, seg_bits)?;
                        matchers.push(curr); curr = matcher;

                        if matchers.iter().find(|matcher| matcher.id == curr.id).is_some() {
                            bail!("symlink loop in recursive glob match: {}", curr.dir.display())
                        }
                    },
                }
            }

            if matchers.is_empty() && let Some(next) = roots.pop() { matchers.push(next) }
        }
    }

    matches.sort_unstable();
    if !glob.is_match_multiple() {
        match matches.len() {
            0 => bail!("non-wildcard glob did not match any files"),
            1 => (),
            2.. => bail!("non-wildcard glob matched multiple files: {matches:#?}"),
        }
    }

    Ok(matches)
}

struct Glob {
    parent_segs: usize,
    base_path: FixedPath,
    floating_paths: Box<[FixedPath]>,
    trailing_sep: bool,
}

struct FixedPath {
    segments: Box<[Segment]>,
}

struct Segment {
    base_str: FixedStr,
    floating_strs: Box<[FixedStr]>,
}

struct FixedStr {
    patterns: Box<[Pattern]>,
}

enum Pattern {
    Lit(Normalized<Box<str>>),
    AnyGraphemes(NonZero<usize>),
}

#[derive(Copy, Clone)]
struct Normalized<T: ops::Deref<Target = str>>(T);

impl Glob {
    fn is_match_multiple(&self) -> bool {
        !self.floating_paths.is_empty() || self.base_path.is_match_multiple()
    }
}

impl FixedPath {
    fn is_match_multiple(&self) -> bool {
        self.segments.iter().any(Segment::is_match_multiple)
    }
}

impl Segment {
    fn match_text(&self, mut text: &[u8]) -> bool {
        {
            let chunk = text.utf8_chunks().next().map(|c| c.valid()).unwrap_or("");
            let mut graphemes = chunk.graphemes(true);
            if !self.base_str.match_prefix(&mut graphemes) { return false }

            let idx = if let Some(b) = graphemes.as_str().as_bytes().first() {
                text.element_offset(b).expect("subref out of range")
            } else if let Some(b) = chunk.as_bytes().last() {
                text.element_offset(b).expect("subref out of range") + 1
            } else { 0 };

            text = &text[idx..];
        }

        let mut floating_strs = self.floating_strs.iter();

        if let Some(last_str) = floating_strs.next_back() {
            let chunk = rev_utf8_chunks(text).next().map(|(v, _)| v).unwrap_or("");
            let mut graphemes = chunk.graphemes(true);
            if !last_str.match_suffix(&mut graphemes) { return false }

            let idx = if let Some(b) = graphemes.as_str().as_bytes().last() {
                text.element_offset(b).expect("subref out of range") + 1
            } else if let Some(b) = chunk.as_bytes().first() {
                text.element_offset(b).expect("subref out of range")
            } else { text.len() };

            text = &text[..idx];
        } else if !text.is_empty() { return false }

        let mut chunks = text.utf8_chunks().map(|c| c.valid());
        let mut graphemes = chunks.next().unwrap_or("").graphemes(true);
        for floating_str in floating_strs {
            loop {
                let mut fork = graphemes.clone();
                if floating_str.match_prefix(&mut fork) {
                    graphemes = fork;
                    break
                } else if graphemes.next().is_none() {
                    let Some(chunk) = chunks.next() else { return false };
                    graphemes = chunk.graphemes(true);
                }
            }
        }

        true
    }

    fn is_match_multiple(&self) -> bool {
        !self.floating_strs.is_empty() || self.base_str.is_match_multiple()
    }
}

impl FixedStr {
    fn match_prefix(&self, graphemes: &mut Graphemes) -> bool {
        for pattern in &self.patterns {
            match pattern {
                Pattern::Lit(lit) => 'pat: {
                    let mut lit = lit.as_deref();
                    for grapheme in graphemes.by_ref() {
                        let Some(rem) = lit.strip_prefix_iter(grapheme.chars()) else { break };
                        lit = rem;

                        if lit.is_empty() { break 'pat }
                    }
                    return false
                },

                Pattern::AnyGraphemes(n)
                    => if graphemes.nth(n.get() - 1).is_none() { return false },
            };
        }

        true
    }

    fn match_suffix(&self, graphemes: &mut Graphemes) -> bool {
        let mut norm_buf = String::with_capacity(4);

        for pattern in self.patterns.iter().rev() {
            match pattern {
                Pattern::Lit(lit) => 'pat: {
                    let mut lit = lit.as_deref();
                    for grapheme in graphemes.rev() {
                        norm_buf.reserve(grapheme.len());
                        let grapheme = Normalized::normalize_to(&mut norm_buf, grapheme.chars());

                        let Some(rem) = lit.strip_suffix(grapheme) else { break };
                        lit = rem;

                        norm_buf.clear();
                        if lit.is_empty() { break 'pat }
                    }
                    return false
                },

                Pattern::AnyGraphemes(n)
                    => if graphemes.rev().nth(n.get() - 1).is_none() { return false },
            };
        }

        true
    }

    fn is_match_multiple(&self) -> bool {
        self.patterns.iter().any(Pattern::is_match_multiple)
    }
}

impl Pattern {
    fn is_match_multiple(&self) -> bool {
        match self {
            Pattern::Lit(_) => false,
            Pattern::AnyGraphemes(_) => true,
        }
    }
}

impl<T: ops::Deref<Target = str> + FromIterator<char>> Normalized<T> {
    fn normalize(text: impl Iterator<Item = char>) -> Self {
        Normalized(normalize_char_iter(text).collect())
    }
}

impl<T: ops::Deref<Target = str>> Normalized<T> {
    fn as_deref(&self) -> Normalized<&str> { Normalized(&self.0) }
    fn is_empty(&self) -> bool { self.0.is_empty() }
}

impl<'b> Normalized<&'b str> {
    fn normalize_to(buf: &'b mut String, text: impl Iterator<Item = char>) -> Self {
        let start = buf.len();
        buf.extend(normalize_char_iter(text));
        Normalized(&buf[start..])
    }

    fn strip_prefix_iter(mut self, prefix: impl Iterator<Item = char>) -> Option<Self> {
        for c in normalize_char_iter(prefix) { self.0 = self.0.strip_prefix(c)? }
        Some(self)
    }

    fn strip_suffix(self, suffix: Normalized<&str>) -> Option<Self> {
        self.0.strip_suffix(suffix.0).map(Normalized)
    }
}

fn normalize_char_iter(text: impl Iterator<Item = char>) -> impl Iterator<Item = char> {
    text.nfd().default_case_fold().nfkd().default_case_fold().nfkd()
}

fn rev_utf8_chunks<'t>(mut text: &'t [u8]) -> impl Iterator<Item = (&'t str, &'t [u8])> {
    iter::from_fn(move || {
        if text.is_empty() { return None }

        let mut end = text.len();
        let mut valid_at = end;
        while end > 0 {
            let (i, valid) = text[..end]
                .iter()
                .copied()
                .rev()
                .enumerate()
                .take(4)
                .filter_map(|(w, b)| if utf8_char_width(b) > w { Some(w) } else { None })
                .map(|w| end - w - 1)
                .map(|i| (i, str::from_utf8(&text[i..end]).is_ok()))
                .next()
                .unwrap_or((end - 1, false));

            end = i;
            if valid { valid_at = i } else { break }
        }

        let valid = str::from_utf8(&text[valid_at..]).expect("invalid utf-8");
        let invalid = &text[end..valid_at];
        text = &text[..end];

        Some((valid, invalid))
    })
}

fn utf8_char_width(first_byte: u8) -> usize {
    const UTF8_CHAR_WIDTH: &[u8; 256] = &[
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2,
        3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3,
        4, 4, 4, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    UTF8_CHAR_WIDTH[first_byte as usize] as usize
}

fn parse(glob: &str) -> anyhow::Result<Glob> {
    if glob.is_empty() { bail!("empty string is not a valid glob") }

    let mut parent_segs = 0;
    let mut trailing_sep = false;

    let mut components = segment(glob)
        .skip_while(|c| if let Ok(Component::Parent) = c { parent_segs += 1; true } else { false })
        .peekable();

    let mut next_res = None;
    let mut fixed_paths = iter::from_fn(|| {
        if let Some(r) = next_res.take() { return Some(r) }

        let mut segments = vec![];
        let eos = loop {
            match components.next() {
                Some(Ok(Component::Seg(seg))) => segments.push(seg),
                Some(Ok(Component::Parent)) => return Some(Err(anyhow!(
                    "parent segments are only allowed at the beginning of glob paths",
                ))),
                Some(Ok(Component::AnyTree)) => {
                    while components
                        .next_if(|c| matches!(c, Ok(Component::AnyTree)))
                        .is_some() { }

                    if let None | Some(Ok(Component::TrailingSep)) = components.peek() {
                        next_res = Some(Ok(FixedPath {
                            segments: Box::new([Segment {
                                base_str: FixedStr { patterns: Box::new([]) },
                                floating_strs: Box::new([FixedStr { patterns: Box::new([]) }]),
                            }]),
                        }));
                    }

                    break false
                },
                Some(Ok(Component::TrailingSep)) => trailing_sep = true,
                Some(Err(e)) => return Some(Err(e)),
                None => break true,
            }
        };

        let segments = segments.into_boxed_slice();
        if segments.is_empty() && eos { None } else { Some(Ok(FixedPath { segments })) }
    });

    let base_path = fixed_paths.next().transpose()?.unwrap_or(FixedPath { segments: Box::new([]) });
    let floating_paths = fixed_paths.collect::<anyhow::Result<_>>()?;
    Ok(Glob { parent_segs, base_path, floating_paths, trailing_sep })
}

enum Component {
    Seg(Segment),
    Parent,
    AnyTree,
    TrailingSep,
}

fn segment(glob: &str) -> impl Iterator<Item = anyhow::Result<Component>> {
    let mut tokens = lex(glob).peekable();
    let mut next_res = None;

    if let Some(Ok(Token::Sep)) = tokens.peek() {
        next_res = Some(Err(anyhow!("glob must be a relative path")))
    }

    iter::from_fn(move || Some(Ok('iter: loop {
        if let Some(r) = next_res.take() { return Some(r) };

        const ONE: NonZero<usize> = NonZero::new(1).unwrap();
        enum PreNormPat<'g> { Lit(&'g str), AnyGraphemes(NonZero<usize>) }

        let mut fixed_strs = vec![];
        let mut patterns = vec![];
        let eos = loop {
            match tokens.next() {
                Some(Ok(Token::Lit(lit))) => patterns.push(PreNormPat::Lit(lit)),
                Some(Ok(Token::AnyGrapheme)) => match patterns.last_mut() {
                    Some(PreNormPat::AnyGraphemes(chars)) => *chars = chars.saturating_add(1),
                    _ => patterns.push(PreNormPat::AnyGraphemes(ONE)),
                },
                Some(Ok(Token::AnyStr)) => {
                    if !patterns.is_empty() {
                        fixed_strs.push(patterns);
                        patterns = vec![];
                    } else if fixed_strs.is_empty() {
                        fixed_strs.push(vec![]);
                    }
                },
                Some(Ok(Token::Sep)) => {
                    if tokens.peek().is_none() { next_res = Some(Ok(Component::TrailingSep)) }
                    break false
                },
                Some(Ok(Token::AnyTree)) => {
                    match tokens.peek() {
                        Some(Ok(Token::Sep) | Err(_)) | None => (),
                        Some(Ok(_)) => return Some(Err(anyhow!(
                            "double wildcard may only be followed by separators",
                        ))),
                    }

                    if fixed_strs.is_empty() && patterns.is_empty() {
                        break 'iter Component::AnyTree
                    } else {
                        return Some(Err(anyhow!(
                            "double wildcard may only be preceded by separators",
                        )))
                    }
                },
                Some(Err(e)) => return Some(Err(e)),
                None => break true,
            }
        };
        if !patterns.is_empty() || !fixed_strs.is_empty() { fixed_strs.push(patterns) }

        if fixed_strs.len() <= 1 {
            match fixed_strs.get(0).map(|v| &v[..]).unwrap_or(&[]) {
                [] => if eos { return None } else { continue },
                [PreNormPat::Lit(".")] => continue,
                [PreNormPat::Lit("..")] => break Component::Parent,
                _ => (),
            }
        }

        let mut fixed_strs = fixed_strs.into_iter().map(|patterns| {
            let mut patterns = patterns.into_iter().peekable();
            let patterns = iter::from_fn(|| match patterns.next() {
                Some(PreNormPat::Lit(lit)) => {
                    let chars = lit.chars().chain({
                        iter::from_fn(|| patterns.next_if_map(|p| match p {
                            PreNormPat::Lit(lit) => Ok(lit.chars()),
                            p => Err(p),
                        }))
                        .flatten()
                    });

                    Some(Pattern::Lit(Normalized::normalize(chars)))
                },
                Some(PreNormPat::AnyGraphemes(n)) => Some(Pattern::AnyGraphemes(n)),
                None => None,
            }).collect();

            FixedStr { patterns }
        });

        let base_str = fixed_strs.next().unwrap_or(FixedStr { patterns: Box::new([]) });
        let floating_strs = fixed_strs.collect();

        break Component::Seg(Segment { base_str, floating_strs })
    })))
}

enum Token<'g> {
    Lit(&'g str),
    AnyGrapheme,
    AnyStr,
    Sep,
    AnyTree,
}

fn lex(mut glob: &str) -> impl Iterator<Item = anyhow::Result<Token<'_>>> {
    iter::from_fn(move || {
        let mut chars = glob.char_indices().peekable();
        let (_, c) = chars.next()?;
        let mut consumed = 0;

        match c {
            '/' => { glob = &glob[1..]; return Some(Ok(Token::Sep)) },
            '?' => {
                glob = &glob[1..];
                return Some(Ok(Token::AnyGrapheme))
            },
            '*' => match chars.next() {
                Some((_, '*')) => {
                    glob = &glob[2..];
                    while let Some(_) = chars.next_if(|&(_, c)| c == '*') {
                        glob = &glob[1..];
                    }
                    return Some(Ok(Token::AnyTree))
                },
                _ => {
                    glob = &glob[1..];
                    return Some(Ok(Token::AnyStr))
                },
            },
            '\\' => match chars.next() {
                Some((_, '?' | '*' | '\\')) => { glob = &glob[1..]; consumed += 1 },
                Some((_, c)) => return Some(Err(anyhow!("invalid escape sequence: {c}"))),
                None => return Some(Err(anyhow!("unfinished escape sequence"))),
            },
            _ => (),
        }

        for (i, c) in chars.map(|(i, c)| (i - consumed, c)) {
            if let '/' | '?' | '*' | '\\' = c {
                let lit = &glob[..i];
                glob = &glob[i..];
                return Some(Ok(Token::Lit(lit)))
            }
        }

        let lit = glob;
        glob = "";
        Some(Ok(Token::Lit(lit)))
    })
}

#[cfg(test)]
mod test {
    #[test]
    fn segment_matching() {
        const CASES: &[(&str, &[u8], bool)] = &[
            ("abcdefg", b"ABCDEFG", true),
            ("abcdefg", b"ABCDEFG ", false),
            ("foo?*?bar", b"fooz\xFFzfbo\xFFar\xFFazjkduibar", true),
            ("k*", b"k\xF0\x9F\x87\xA8\xF0\x9F\x87\xA6\xFFv", true),
            ("k\u{1F1E8}*", b"k\xF0\x9F\x87\xA8\xF0\x9F\x87\xA6\xFFv", false),
            ("k\u{1F1E8}\u{1F1E6}*", b"k\xF0\x9F\x87\xA8\xF0\x9F\x87\xA6\xFFv", true),
            ("k\u{1F1E8}\u{1F1E6}*v", b"k\xF0\x9F\x87\xA8\xF0\x9F\x87\xA6\xFFv", true),
            ("?", b"\xFF", false),
            ("a*b*c", b"azzxbyyyc", true),
            ("a*b*c", b"azzxbyyyc\0", false),
            ("*foo*bar*baz*", b"obfobbfoozazofbrbabarfzrobaofbaazzbazf", true),
            ("*.jpg", b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF.JPG", true),
            ("\u{1C6}\u{2460}ß.xml", b"d\xC5\xBE1Ss.XmL", true),
        ];

        for &(glob, text, expected) in CASES {
            let g = super::parse(glob).expect("failed to parse glob");
            if g.parent_segs != 0 || !g.floating_paths.is_empty() || g.trailing_sep {
                panic!("single-segment glob parsed as multiple segments: {glob:?}");
            }

            let [seg] = &*g.base_path.segments else {
                panic!("single-segment glob parsed as multiple segments: {glob:?}");
            };

            assert_eq!(
                seg.match_text(text.as_ref()), expected,
                "match({glob:?}, {:?})", String::from_utf8_lossy(text),
            );
        }
    }

    #[test]
    fn rev_utf8_chunks() {
        const CASES: &[(&[u8], &[(&str, &[u8])])] = &[
            (b"\xE1\xA0 ", &[(" ", b"\xE1\xA0")]),
            (b"a\xF0\x9F\xAB\xAAb\xF0\x80\x80\x80cd", &[
                ("cd", b"\xF0\x80\x80\x80"),
                ("a\u{1FAEA}b", b""),
            ]),
            (b"\x80foo\xFFbar\xF7\xF8baz\x80\x80\x80", &[
                ("", b"\x80"),
                ("", b"\x80"),
                ("", b"\x80"),
                ("baz", b"\xF8"),
                ("", b"\xF7"),
                ("bar", b"\xFF"),
                ("foo", b"\x80"),
            ]),
        ];

        for (text, expected) in CASES {
            let chunks = super::rev_utf8_chunks(text).collect::<Vec<_>>();
            assert_eq!(&chunks, expected, "text: {text:?}");
        }
    }
}
