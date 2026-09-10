mod app;
mod guitheme;

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

        // Window-independent commands. Element-level handlers only fire along
        // the focused subtree's bubble path (a focused modal bypasses app
        // content), while these fire for every dispatch regardless of focus.
        // The dialog's open guard dedups against the content-root handler.
        cx.on_action(|_: &app::OpenSettings, cx: &mut App| {
            if let Some(handle) = cx.windows().into_iter().next() {
                let _ = handle.update(cx, |_, window, cx| {
                    if let Some(view) = app::app_view(cx) {
                        app::SqlHighlandView::open_settings_dialog(&view, window, cx);
                    }
                });
            }
        });
        // Register the keystroke in the application keymap, not only from
        // the view constructor. This makes the binding active before any
        // editor/input key context is focused.
        cx.bind_keys([KeyBinding::new("cmd-,", app::OpenSettings, None)]);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(780.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.set_window_title("SQLHighland");
                // Apply saved theme now that a window exists (System mode
                // needs the OS appearance).
                guitheme::apply_preferences(&sqlhighland::config::Preferences::load(), Some(window), cx);
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
