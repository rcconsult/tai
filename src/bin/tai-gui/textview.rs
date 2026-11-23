//! `/show` of a text or code file: ppxai answers with a `FileViewResult`
//! that carries the file's text, and an `open_viewer` side effect. The web
//! app opens a read-only code view in its side panel
//! (web/shared/side-effects.js, `open_viewer`); tai-gui opens a tab in the
//! Preview stack, as `/show` of an image does, with the text highlighted
//! by syntect (egui_extras, the highlighter egui_commonmark already uses).

use std::path::{Path, PathBuf};

use eframe::egui;

/// The most text a tab shows. ppxai sends the whole file; a longer one is
/// cut here, with a note, so one `/show` can't stall the window.
const SHOWN_BYTES: usize = 512 * 1024;

/// A file open in a viewer tab.
pub struct TextFile {
    pub path: PathBuf,
    text: String,
    /// Lines in `text`, for the line numbers.
    lines: usize,
    /// What syntect goes by: the file's extension, else ppxai's language.
    syntax: String,
    /// The whole file was longer than `text`.
    cut: Option<usize>,
}

impl TextFile {
    /// The file `path` with `text`, which ppxai read; `language` is ppxai's
    /// name for it, used when the file has no extension.
    pub fn new(path: &str, text: &str, language: Option<&str>) -> Self {
        let path = PathBuf::from(path);
        let syntax = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .or_else(|| language.map(str::to_string))
            .unwrap_or_else(|| "txt".into());
        let (text, cut) = if text.len() > SHOWN_BYTES {
            let mut end = SHOWN_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            // Whole lines only.
            let end = text[..end].rfind('\n').map_or(end, |n| n + 1);
            (text[..end].to_string(), Some(text.len()))
        } else {
            (text.to_string(), None)
        };
        let lines = text.lines().count().max(1);
        Self {
            path,
            text,
            lines,
            syntax,
            cut,
        }
    }

    /// The tab's title: the file's name.
    pub fn title(&self) -> String {
        name(&self.path)
    }

    pub fn ui(&self, ui: &mut egui::Ui) {
        ui.weak(self.path.display().to_string());
        if let Some(size) = self.cut {
            ui.weak(format!(
                "The first {} KB of {} KB.",
                self.text.len() / 1024,
                size / 1024
            ));
        }
        ui.separator();
        let theme = egui_extras::syntax_highlighting::CodeTheme::from_style(ui.style());
        // Memoized by egui_extras, so it's laid out once, not every frame.
        let job = egui_extras::syntax_highlighting::highlight(
            ui.ctx(),
            ui.style(),
            &theme,
            &self.text,
            &self.syntax,
        );
        let width = self.lines.to_string().len();
        let numbers = (1..=self.lines)
            .map(|n| format!("{n:>width$}"))
            .collect::<Vec<_>>()
            .join("\n");
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.add(
                    egui::Label::new(egui::RichText::new(numbers).monospace().weak())
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(false),
                );
                ui.add_space(8.0);
                ui.add(
                    egui::Label::new(job)
                        .wrap_mode(egui::TextWrapMode::Extend)
                        .selectable(true),
                );
            });
        });
    }
}

fn name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_syntax_comes_from_the_extension_else_the_language() {
        assert_eq!(TextFile::new("/w/a.rs", "", Some("python")).syntax, "rs");
        assert_eq!(
            TextFile::new("/w/Makefile", "", Some("makefile")).syntax,
            "makefile"
        );
        assert_eq!(TextFile::new("/w/notes", "", None).syntax, "txt");
        assert_eq!(TextFile::new("/w/a.rs", "", None).title(), "a.rs");
    }

    #[test]
    fn a_long_file_is_cut_at_a_line() {
        let line = "é".repeat(99) + "\n"; // 199 bytes
        let text = line.repeat(SHOWN_BYTES / line.len() + 10);
        let file = TextFile::new("/w/big.txt", &text, None);
        assert_eq!(file.cut, Some(text.len()));
        assert!(file.text.len() <= SHOWN_BYTES);
        assert!(file.text.ends_with('\n'));
        assert_eq!(file.lines, SHOWN_BYTES / line.len());
        let small = TextFile::new("/w/s.txt", "a\nb", None);
        assert_eq!((small.cut, small.lines), (None, 2));
    }
}
