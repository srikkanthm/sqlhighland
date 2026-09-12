use sqlhighland::{app, guitheme};

use gpui_kit::component::*;
use gpui_kit::*;

fn main() {
    // AllAssets embeds the complete Lucide catalog. The default `Assets`
    // bundle only covers a subset, which leaves icons like Database/Plug
    // rendering blank.
    let app = gpui_kit::application().with_assets(gpui_kit::assets::AllAssets);

    app.run(move |cx| {
        // This must be called before using any GPUI Component features.
        gpui_kit::init(cx);
        guitheme::register_themes(cx);

        // Window-independent commands. Quit and Settings live ONLY here
        // (no element-level duplicates): a second registration fires the
        // action twice per keypress. These fire for every dispatch
        // regardless of focus. Bodies are deferred: keypress dispatch runs
        // while the window is taken, so a nested handle.update here would
        // fail silently — the deferred body runs after the event batch,
        // when the window can be re-taken.
        cx.on_action(|_: &app::OpenSettings, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        if let Some(view) = app::app_view(cx) {
                            // Same toggle as the view-level entry, inside
                            // view.update (safe here: nothing leases the view).
                            let active = window.has_active_dialog(cx);
                            let toggle_off = view.update(cx, |this, _| {
                                this.note_dialog_open_for_settings(active)
                            });
                        if !toggle_off {
                            app::SqlHighlandView::open_settings_dialog(&view, window, cx);
                        } else if window.has_active_dialog(cx) {
                            window.close_dialog(cx);
                        }
                        }
                    });
                }
            });
        });
        // Register the keystroke in the application keymap, not only from
        // the view constructor. This makes the binding active before any
        // editor/input key context is focused.
        cx.bind_keys([KeyBinding::new("cmd-,", app::OpenSettings, None)]);
        cx.bind_keys([KeyBinding::new("cmd-q", app::Quit, None)]);
        cx.on_action(|_: &app::Quit, cx: &mut App| {
            // Deferred for the same window-take reason as above.
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        if let Some(view) = app::app_view(cx) {
                            view.update(cx, |this, cx| {
                                this.request_quit(window, cx);
                            });
                        } else {
                            cx.quit();
                        }
                    });
                } else {
                    cx.quit();
                }
            });
        });
        // Native application menu. Without this macOS installs no menu bar
        // entry at all, which is why ⌘Q previously did nothing: there was
        // no Quit item to invoke. Key equivalents render from the keymap.
        cx.set_menus(vec![
            Menu {
                name: "SQLHighland".into(),
                items: vec![
                    MenuItem::action("Preferences…", app::OpenSettings),
                    MenuItem::Separator,
                    MenuItem::action("Quit", app::Quit),
                ],
                disabled: false,
            },
            Menu {
                name: "File".into(),
                items: vec![
                    MenuItem::action("Open SQL File…", app::OpenSql),
                    MenuItem::action("Save", app::SaveSql),
                    MenuItem::action("Save As…", app::SaveSqlAs),
                ],
                disabled: false,
            },
        ]);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(780.)), cx)),
            // Custom title bar rendered by the app (themed) instead of the
            // native one (which follows the OS appearance, not app themes).
            ..TitleBar::window_options()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.set_window_title("SQLHighland");
                // Apply saved theme now that a window exists (System mode
                // needs the OS appearance).
                guitheme::apply_preferences(
                    &sqlhighland::config::Preferences::load(),
                    Some(window),
                    cx,
                );
                let view = cx.new(|cx| app::SqlHighlandView::new(window, cx));
                // Stash for window-level handlers (global Cmd+,), which have
                // a window but no view handle.
                cx.set_global(app::AppView(view.downgrade()));
                // The first level on the window must be a Root.
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}
