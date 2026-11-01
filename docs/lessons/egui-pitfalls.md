# egui pitfalls

Hazards in tai-gui's use of egui and eframe (ADR 0011). Each one holds on
every OS unless it says otherwise, and can be checked with grep against
the named file.

## egui-winit turns Ctrl+C, Ctrl+X and Ctrl+V into clipboard events

egui-winit 0.36.2 (`src/lib.rs`, `is_copy_command`, `is_cut_command`,
`is_paste_command`) maps the Copy, Cut and Paste chords to
`Event::Copy`, `Event::Cut` and `Event::Paste(text)` and returns without
sending a key event. The chords are Ctrl+C/X/V (Cmd on macOS) plus, on
Windows, Ctrl+Insert, Shift+Delete and Shift+Insert. Paste has already
read the clipboard and turned CRLF into LF.

A terminal pane never sees Ctrl+C as a key, so it can't send SIGINT
unless it handles the event itself. tai-gui's terminal pane sends Ctrl+C
and Ctrl+X on to the shell off macOS, where those chords belong to the
shell (`src/shortcuts.rs`), and pastes `Event::Paste`
(`src/bin/tai-gui/terminal.rs`, `handle_event`).

## A blinking text cursor keeps egui drawing

With a text box focused, egui's blinking cursor kept requesting repaints:
about 30 frames a second and 6-12% of a core on the Windows host, in a
window that otherwise draws nothing while idle. tai-gui turns blinking
off (`style.visuals.text_cursor.blink = false`, in `src/bin/tai-gui/main.rs`).
`TAI_FRAME_STATS=1` logs egui's repaint causes, which is how it showed
up: `egui/src/text_selection/visuals.rs`.
