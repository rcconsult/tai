# Issue draft: egui_commonmark's copy icon covers the end of a code line

Not filed (owner's call, 2026-11-21: keep it as an open item for now).
This is the issue tai would post to
[lampsitter/egui_commonmark](https://github.com/lampsitter/egui_commonmark),
kept here for reference. tai-gui's chat shows it: zoomed in, a code
block's copy icon sits on top of the end of the first line. Before
filing, check that it's still present upstream and that nobody has
reported it already (on 2026-11-21 the closest were #114, icon
customisation, and #112, the button widening its parent). Update the
version and line references if they've moved.

---

## Code block copy button overlaps the code when a line wraps to the right edge

**Version:** egui_commonmark 0.25.0 / egui_commonmark_backend 0.25.0 (egui 0.36.2). The code is the same in 0.26.0.

**What happens:** in `elements::code_block`, the `TextEdit` gets `desired_width(max_width)`, so the layouter wraps code lines right up to the frame's right edge. The copy button is then placed inside the frame's top-right corner:

```rust
let position = egui::pos2(
    frame_rect.right_top().x - spacing.icon_width * 0.5 - spacing.button_padding.x,
    frame_rect.right_top().y + spacing.button_padding.y * 2.0,
);
```

When the first line is long enough to reach the edge, either because it wraps or because it just fits, the icon is drawn on top of its last characters. A larger font (for example `ctx.set_zoom_factor(1.5)`) makes this happen with shorter lines.

**To reproduce:** render a fenced code block whose first line is longer than the viewer is wide, for example:

````markdown
```rust
fn main() { println!("a line long enough to wrap at the right edge of the viewer, so its end sits under the copy icon"); }
```
````

**Possible fix:** keep the code clear of the icon, for example by giving the `TextEdit` a right margin of about `icon_width + 2 * button_padding.x`, so the layouter wraps before the icon:

```rust
let output = egui::TextEdit::multiline(&mut text)
    .layouter(layouter)
    .desired_width(max_width)
    .margin(egui::Margin { right: (spacing.icon_width + 2.0 * spacing.button_padding.x) as i8, ..egui::Margin::symmetric(4, 2) })
    .desired_rows(1)
    .show(ui);
```

(Reserving the space only on the first line would need a different layout; a uniform right margin is the simplest.) I'm happy to send a PR if this approach suits you.
