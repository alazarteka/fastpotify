//! Keyboard shortcuts.

use egui::{Key, Modifiers};

use crate::app::App;
use crate::model::{Action, Dialog, Page};

pub fn handle(app: &mut App, ctx: &egui::Context) {
    let typing = ctx.memory(|memory| memory.focused().is_some());
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        let mut key = |modifiers: Modifiers, key: Key, action: Action| {
            if input.consume_key(modifiers, key) {
                actions.push(action);
            }
        };
        key(Modifiers::COMMAND, Key::F, Action::FocusSearch);
        key(Modifiers::COMMAND, Key::B, Action::ToggleSidebar);
        key(Modifiers::COMMAND, Key::Comma, Action::Open(Page::Settings));
        key(Modifiers::COMMAND, Key::Q, Action::Quit);
        // winit installs its own macOS app menu, whose Hide item owns Cmd+H
        // before the window is offered the key.
        if cfg!(target_os = "macos") {
            key(
                Modifiers::COMMAND | Modifiers::SHIFT,
                Key::H,
                Action::Open(Page::Home),
            );
        } else {
            key(Modifiers::COMMAND, Key::H, Action::Open(Page::Home));
        }
        key(Modifiers::COMMAND, Key::L, Action::Open(Page::LikedSongs));
        key(
            Modifiers::COMMAND,
            Key::Slash,
            Action::ShowDialog(Dialog::Shortcuts),
        );
        key(Modifiers::ALT, Key::ArrowLeft, Action::Back);
        key(Modifiers::ALT, Key::ArrowRight, Action::Forward);
        key(Modifiers::COMMAND, Key::ArrowLeft, Action::Previous);
        key(Modifiers::COMMAND, Key::ArrowRight, Action::Next);
        key(Modifiers::COMMAND, Key::ArrowUp, Action::VolumeBy(5));
        key(Modifiers::COMMAND, Key::ArrowDown, Action::VolumeBy(-5));
        key(
            Modifiers::COMMAND | Modifiers::SHIFT,
            Key::A,
            Action::OpenUri("artist".into()),
        );
        key(
            Modifiers::COMMAND | Modifiers::SHIFT,
            Key::B,
            Action::OpenUri("album".into()),
        );
        // Cmd+Shift+Q is Log Out, taken by the window server.
        if cfg!(target_os = "macos") {
            key(Modifiers::COMMAND, Key::U, Action::ToggleQueuePanel);
        } else {
            key(
                Modifiers::COMMAND | Modifiers::SHIFT,
                Key::Q,
                Action::ToggleQueuePanel,
            );
        }
        if !typing {
            key(
                Modifiers::NONE,
                Key::Questionmark,
                Action::ShowDialog(Dialog::Shortcuts),
            );
            key(
                Modifiers::SHIFT,
                Key::Questionmark,
                Action::ShowDialog(Dialog::Shortcuts),
            );
            key(Modifiers::SHIFT, Key::ArrowLeft, Action::SeekBy(-10_000));
            key(Modifiers::SHIFT, Key::ArrowRight, Action::SeekBy(10_000));
            key(Modifiers::NONE, Key::Space, Action::TogglePlay);
            key(Modifiers::NONE, Key::M, Action::ToggleMute);
            key(Modifiers::NONE, Key::S, Action::ToggleShuffle);
            key(Modifiers::NONE, Key::R, Action::CycleRepeat);
            key(Modifiers::NONE, Key::Q, Action::ToggleQueuePanel);
            key(Modifiers::NONE, Key::L, Action::ToggleLyricsPanel);
            key(Modifiers::NONE, Key::Slash, Action::FocusSearch);
        }
    });
    // Resolve the "open current artist/album" placeholders.
    for action in actions {
        match action {
            Action::OpenUri(kind) if kind == "artist" => {
                if let Some(id) = app
                    .now_playing()
                    .and_then(|now| now.artists.first().and_then(|artist| artist.id.clone()))
                {
                    app.actions.push(Action::Open(Page::Artist(id)));
                }
            }
            Action::OpenUri(kind) if kind == "album" => {
                if let Some(now) = app.now_playing() {
                    if let Some(id) = now.album_id {
                        app.actions.push(Action::Open(Page::Album(id)));
                    } else if let Some(id) = now.show_id {
                        app.actions.push(Action::Open(Page::Show(id)));
                    }
                }
            }
            other => app.actions.push(other),
        }
    }
    if ctx.input(|input| input.key_pressed(Key::Escape)) {
        if app.dialog.is_some() {
            app.actions.push(Action::CloseDialog);
        } else if app.show_devices {
            app.show_devices = false;
        }
    }
}

pub const SHORTCUTS: &[(&str, &str)] = &[
    ("Space", "Play or pause"),
    ("Ctrl+←  /  Ctrl+→", "Previous or next"),
    ("Shift+←  /  Shift+→", "Seek 10 seconds"),
    ("Ctrl+↑  /  Ctrl+↓", "Volume up or down"),
    ("M", "Mute or unmute"),
    ("S", "Toggle shuffle"),
    ("R", "Cycle repeat"),
    ("Q", "Show the queue"),
    ("L", "Show the lyrics"),
    ("Ctrl+F  or  /", "Search"),
    ("Ctrl+B", "Show or hide the sidebar"),
    ("Alt+←  /  Alt+→", "Back or forward"),
    (
        if cfg!(target_os = "macos") {
            "Ctrl+Shift+H"
        } else {
            "Ctrl+H"
        },
        "Home",
    ),
    ("Ctrl+L", "Liked Songs"),
    ("Ctrl+Shift+A", "Go to the playing artist"),
    ("Ctrl+Shift+B", "Go to the playing album"),
    ("Ctrl+,", "Settings"),
    ("Ctrl+/ or ?", "Keyboard shortcuts"),
    ("Ctrl+Q", "Quit"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppOptions;
    use crate::backend::Waker;
    use crate::paths::AppDirs;
    use crate::settings::Settings;

    fn app() -> App {
        let root = std::env::temp_dir().join(format!(
            "fastpotify-question-shortcut-test-{}",
            std::process::id()
        ));
        App::new(
            &Waker::default(),
            AppDirs {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
            },
            Settings::default(),
            AppOptions {
                media_controls: false,
                tray: false,
            },
        )
    }

    fn pass(ctx: &egui::Context, events: Vec<egui::Event>, run_ui: impl FnMut(&mut egui::Ui)) {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            events,
            ..Default::default()
        };
        let mut output = ctx.run_ui(input, run_ui);
        output.textures_delta.clear();
    }

    fn questionmark() -> egui::Event {
        egui::Event::Key {
            key: Key::Questionmark,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::SHIFT,
        }
    }

    #[test]
    fn questionmark_opens_shortcuts_except_while_typing() {
        let ctx = egui::Context::default();
        let mut app = app();

        pass(&ctx, vec![questionmark()], |ui| handle(&mut app, ui.ctx()));
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::ShowDialog(Dialog::Shortcuts)))
        );

        app.actions.clear();
        let mut text = String::new();
        pass(&ctx, Vec::new(), |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        assert!(ctx.memory(|memory| memory.focused().is_some()));

        pass(&ctx, vec![questionmark()], |ui| handle(&mut app, ui.ctx()));
        assert!(app.actions.is_empty());
        let _ = app.shutdown();
    }
}
