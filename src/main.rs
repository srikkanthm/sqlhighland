mod app;

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

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(780.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.set_window_title("SQLHighland");
                let view = cx.new(|cx| app::SqlHighlandView::new(window, cx));
                // The first level on the window must be a Root.
                cx.new(|cx| Root::new(view, window, cx).bg(cx.theme().background))
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}
