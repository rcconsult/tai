# Bundled fonts

Both fonts are compiled into the binary (`src/fonts.rs`).

| File | Source | Licence |
|---|---|---|
| `JetBrainsMono-Medium.ttf` | uzaaft/libghostty-rs `example/ghostling_rs/`, tag v0.2.2 (a3a9676, "release: 0.2.2") | SIL OFL 1.1 (`OFL.txt`) |
| `SymbolsNerdFontMono-Regular.ttf` | ryanoasis/nerd-fonts release v3.5.1, asset `NerdFontsSymbolsOnly.tar.xz` | Font: MIT (`SymbolsNerdFont-LICENSE`). Icons: per set, see below |

sha256:

```
31c92d01a8a08528b718a43addf0ad3df0af2ca4b7b3290a452f70f358e14d3d  JetBrainsMono-Medium.ttf
fe471e538392f51910faab985fa8e192a39dd3426125edd15b71b3680df0e749  SymbolsNerdFontMono-Regular.ttf
01172f37db8543edb102e5cb5c64101c9f4686630804d49b419aa07b23a69996  NerdFontsSymbolsOnly.tar.xz (the release asset)
```

`SymbolsNerdFont-README.md` is the README from the same archive. Its
"Icon sets" table gives each icon set's upstream and licence: MIT,
Apache 2.0, OFL 1.1, CC BY 4.0 (Codicons, Font Awesome; attribution
required) and, for Font Logos, "unlicensed". Upstream, lukas-w/font-logos
is under the Unlicense (public domain), with the note that the logos are
trademarks of their owners.

## How fallback works

JetBrains Mono draws everything it has. A codepoint it lacks is drawn
with Symbols Nerd Font Mono if that font has it. The icon is scaled to
one cell, or to two when the next cell is blank (Ghostty does the same),
and centred on the row. Emoji, CJK and system-font discovery aren't
handled yet.
