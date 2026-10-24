# Bundled fonts

All three fonts are compiled into the binary (`src/fonts.rs`).

| File | Source | Licence |
|---|---|---|
| `JetBrainsMono-Medium.ttf` | uzaaft/libghostty-rs `example/ghostling_rs/`, tag v0.2.2 (a3a9676, "release: 0.2.2") | SIL OFL 1.1 (`OFL.txt`) |
| `SymbolsNerdFontMono-Regular.ttf` | ryanoasis/nerd-fonts release v3.5.1, asset `NerdFontsSymbolsOnly.tar.xz` | Font: MIT (`SymbolsNerdFont-LICENSE`). Icons: per set, see below |
| `IosevkaFixedSS01-Medium.ttf` | be5invis/Iosevka release v34.9.0 (2487637, "Release 34.9.0"), asset `PkgTTF-Unhinted-IosevkaFixedSS01-34.9.0.zip` | SIL OFL 1.1 (`Iosevka-LICENSE.md`, the repo's `LICENSE.md` at that tag) |

sha256:

```
31c92d01a8a08528b718a43addf0ad3df0af2ca4b7b3290a452f70f358e14d3d  JetBrainsMono-Medium.ttf
fe471e538392f51910faab985fa8e192a39dd3426125edd15b71b3680df0e749  SymbolsNerdFontMono-Regular.ttf
01172f37db8543edb102e5cb5c64101c9f4686630804d49b419aa07b23a69996  NerdFontsSymbolsOnly.tar.xz (the release asset)
999c3254e382082f1d37eb6e390a60feeff47f204b0366afdac06f977315c1a4  IosevkaFixedSS01-Medium.ttf
0e4b5fb8b5346272b9751340dae8ca61eddf9f1b390d6d429834987f552871e1  PkgTTF-Unhinted-IosevkaFixedSS01-34.9.0.zip (the release asset)
```

`SymbolsNerdFont-README.md` is the README from the same archive. Its
"Icon sets" table gives each icon set's upstream and licence: MIT,
Apache 2.0, OFL 1.1, CC BY 4.0 (Codicons, Font Awesome; attribution
required) and, for Font Logos, "unlicensed". Upstream, lukas-w/font-logos
is under the Unlicense (public domain), with the note that the logos are
trademarks of their owners.

Iosevka is there for symbols only, so the build doesn't matter for
letters. SS01 (a stylistic set) is used because its files are about a
sixth the size of plain Iosevka Fixed's: 1.1 MB against 6.7 MB for
Medium. Its character map differs from plain Iosevka Fixed in 3
codepoints. Unhinted, because fontdue ignores hinting. Medium, to match
JetBrains Mono's weight.

## How fallback works

Unicode spaces (U+00A0, U+2000-200A, U+202F, U+205F, U+3000) draw as
empty cells with no font lookup. Otherwise JetBrains Mono draws
everything it has. A codepoint it lacks is drawn with the first of
Symbols Nerd Font Mono and Iosevka Fixed that has a glyph for it (glyph
index not 0). Fallback glyphs are scaled to one cell, or to two when
the next cell is blank (Ghostty does the same), never taller than
JetBrains Mono's line, and centred on the row. Iosevka's line height
caps its glyphs at one cell.

Iosevka covers general symbols such as ⎈ U+2388 (kube-ps1) and ⏵ U+23F5,
and all of braille (U+2800-28FF). Still missing: emoji, including
☸ U+2638, and CJK. System-font discovery for those isn't handled yet.
