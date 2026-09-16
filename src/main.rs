use std::borrow::Cow;

use sqlhighland::{app, guitheme};

use gpui_kit::component::*;
use gpui_kit::*;

/// App asset source: the kit's embedded icon catalog plus SQLHighland's own
/// app icon. The icon is embedded from `assets/icon/icon-256.png` and served
/// at `sqlhighland-icon.png`, so the About page can render it with
/// `img("sqlhighland-icon.png")` without shipping loose files.
struct AppAssets;

impl gpui::AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if path == "sqlhighland-icon.png" {
            return Ok(Some(Cow::Borrowed(include_bytes!(
                "../assets/icon/icon-256.png"
            ))));
        }
        gpui_kit::assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        gpui_kit::assets::AllAssets.list(path)
    }
}

fn main() {
    // AllAssets embeds the complete Lucide catalog. The default `Assets`
    // bundle only covers a subset, which leaves icons like Database/Plug
    // rendering blank. `AppAssets` layers our own icon on top.
    //
    // LastWindowClosed makes the red close button quit the single-window app
    // (macOS defaults to Explicit, which would leave it running windowless).
    // The view's `on_window_should_close` guard runs first.
    let app = gpui_kit::application()
        .with_assets(AppAssets)
        .with_quit_mode(QuitMode::LastWindowClosed);

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
                            let toggle_off = view
                                .update(cx, |this, _| this.note_dialog_open_for_settings(active));
                            if !toggle_off {
                                let controls = view.read(cx).settings_controls();
                                app::SqlHighlandView::open_settings_dialog(
                                    &view, controls, window, cx, None,
                                );
                            } else if window.has_active_dialog(cx) {
                                window.close_dialog(cx);
                            }
                        }
                    });
                }
            });
        });
        // App menu "About SQLHighland": opens the Settings dialog focused on
        // its About page. Same deferred/window-take discipline as above.
        cx.on_action(|_: &app::OpenAbout, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        if let Some(view) = app::app_view(cx) {
                            view.update(cx, |this, cx| {
                                this.open_about(window, cx);
                            });
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
        // Tab commands live here too (same single-path + defer design as
        // above): element-level duplicates are gone, and dialogs render
        // outside the sidebar/main roots, so element handlers never see
        // dialog-focused keypresses. Active-tab snapshot happens at
        // execution time, after the take is released.
        cx.on_action(|_: &app::NewTab, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        if let Some(view) = app::app_view(cx) {
                            view.update(cx, |this, cx| {
                                this.new_tab_command(window, cx);
                            });
                        }
                    });
                }
            });
        });
        cx.on_action(|_: &app::CloseTab, cx: &mut App| {
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        if let Some(view) = app::app_view(cx) {
                            view.update(cx, |this, cx| {
                                this.close_active_tab_command(window, cx);
                            });
                        }
                    });
                }
            });
        });
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
        // Native application menu (defined in app.rs, covered by a menu
        // test). Without this macOS installs no menu bar entry at all,
        // which is why ⌘Q previously did nothing: there was no Quit item
        // to invoke.
        cx.set_menus(app::app_menus());

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(780.)), cx)),
            // Floor so the toolbars/headers can't be squeezed past the point
            // where their responsive collapse can help.
            window_min_size: Some(size(px(640.), px(480.))),
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
