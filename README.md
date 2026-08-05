# `build-content-pack`

`build-content-pack` is a command-line tool that builds SMAPI content packs (Content Patcher, etc)
from source files. Currently supports transcoding TOML files to JSON files and rendering SVG images
to PNG images.

## Installation

Install the Rust programming language, then run:

```
cargo install --git https://github.com/Esper89/stardew-build-content-pack
```

## Usage

For a list of command-line arguments, run with `--help`.

Built content packs can be exported to a directory or to a `.zip` archive. Directory export supports
caching output files to avoid re-processing the same file (e.g. re-rendering the same SVG) if the
inputs have not changed.

TOML files can include other text files in them with `##include other-file.toml` at the start of a
line. Other uses of `##` at the start of a line (for example, in a string) can be escaped with
`###`. This feature supports basic glob matching.

Directories can be ignored by placing a file named `.content-pack-skip` in the directory.

## License

Copyright © 2026 Esper Thomson

This program is free software: you can redistribute it and/or modify it under the terms of version
3 of the GNU Affero General Public License as published by the Free Software Foundation.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero
General Public License for more details.

You should have received a copy of the GNU Affero General Public License along with this program.
If not, see <https://www.gnu.org/licenses>.

Additional permission under GNU AGPL version 3 section 7

If you modify this Program, or any covered work, by linking or combining it with Stardew Valley (or
a modified version of that program), containing parts covered by the terms of its license, the
licensors of this Program grant you additional permission to convey the resulting work.
