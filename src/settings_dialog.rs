//! Settings dialog: theme/appearance/completion/font/results/export/about.
//!
//! Extracted from `app.rs` (refactor Phase 1); behavior unchanged. Rows
//! apply live, persist to preferences.toml, and notify the view for a
//! full re-render (GPUI only repaints dirty views).

use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{img, px, App, Context, Entity, ObjectFit, Window};
use gpui_kit::component::button::Button;
use gpui_kit::component::setting::{
    RenderOptions, SelectIndex, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;

use crate::app::{app_view, SqlHighlandView};
use crate::config::{CompleteMode, Preferences, SavedConfig, TabsManifest, THEME_LIST};

/// Index of the "About" page within the `Settings::pages` list built in
/// [`SqlHighlandView::open_settings_dialog`] (Themes, Editor, Results,
/// About). Keep in sync if a page is inserted before it.
const ABOUT_PAGE_IX: usize = 3;

/// Asset path served by `AppAssets` in `main.rs` (the embedded app icon).
const ABOUT_ICON: &str = "sqlhighland-icon.png";

/// Makes each targeted "About" open use a fresh `Settings` keyed state, so it
/// always starts on the About page. The kit's `default_selected_index` only
/// applies when the state is first created.
static ABOUT_DIALOG_SEQ: AtomicU64 = AtomicU64::new(0);

impl SqlHighlandView {
    /// Settings dialog: theme family + appearance mode. Selections apply
    /// live, persist to preferences.toml, and close the dialog (menu-like).
    /// Owns the Cmd+, toggle bookkeeping (direct field access: this runs
    /// under the action listener's lease, so Entity::update would panic).
    pub(crate) fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.take_settings_toggle(window, cx) {
            return;
        }
        Self::open_settings_dialog(&cx.entity(), window, cx, None);
    }

    /// App-menu "About SQLHighland": open the Settings dialog on its About
    /// page, replacing any dialog already on screen.
    /// Public only for main.rs (the binary is a separate crate); not part of
    /// the app's UI surface.
    pub fn open_about(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            window.close_dialog(cx);
        }
        self.note_dialog_open_for_settings(window.has_active_dialog(cx));
        Self::open_settings_dialog(&cx.entity(), window, cx, Some(ABOUT_PAGE_IX));
    }

    /// Cmd+, toggle decision shared by the view-level entry (above) and the
    /// global App::on_action entry in main.rs (which mirrors it inside a
    /// view.update — safe there, no outer lease). Returns true when Settings
    /// is the top dialog and has just been dismissed: the caller opens
    /// nothing. Otherwise records the new opening and the caller stacks
    /// Settings on top, so the key always opens it instead of killing
    /// another popup. Needs &mut Window only for has_active_dialog; the
    /// App borrow is unused (kept for call-site symmetry).
    fn take_settings_toggle(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let top = self.note_dialog_open_for_settings(window.has_active_dialog(cx));
        if top {
            window.close_dialog(cx);
        }
        top
    }

    /// Core toggle step for the global entry point (main.rs), which cannot
    /// use take_settings_toggle for lack of &mut self. Same contract.
    /// Public only for main.rs; not part of the app's UI surface.
    pub fn note_dialog_open_for_settings(&mut self, active: bool) -> bool {
        let top_is_settings = self.settings_seq.get() == Some(self.dialog_seq.get()) && active;
        self.dialog_seq.set(self.dialog_seq.get() + 1);
        if top_is_settings {
            self.settings_seq.set(None);
        } else {
            self.settings_seq.set(Some(self.dialog_seq.get()));
        }
        top_is_settings
    }

    /// Save preferences, reporting failure on the status bar instead of
    /// losing it silently. Resolves the view through the AppView global
    /// so settings/dialog callbacks (which own no lease and often no
    /// view handle) stay one-liners. Falls back to stderr where no
    /// window/view exists (e.g. headless tests).
    fn save_prefs_status(prefs: &Preferences, cx: &mut App) {
        if let Err(e) = prefs.save() {
            if let Some(view) = app_view(cx) {
                view.update(cx, |this, cx| {
                    this.status = format!("Preferences save failed: {e:#}").into();
                    cx.notify();
                });
            } else {
                crate::logging::error(format!("preferences save failed: {e:#}"));
            }
        }
    }

    /// Settings dialog as an associated function so the global App::on_action
    /// handler (which has a window but no view handle) can open it too.
    /// Pure open: toggle bookkeeping lives with the callers (see above).
    /// Rows apply, notify the view for a full re-render (GPUI only repaints
    /// dirty views — window refreshes alone reuse cached ones), then close.
    pub fn open_settings_dialog(
        view: &Entity<SqlHighlandView>,
        window: &mut Window,
        cx: &mut App,
        initial_page: Option<usize>,
    ) {
        // Owned for the 'static dialog builder below.
        let view = view.clone();
        // A targeted open (About) uses a unique id so the kit builds fresh
        // state on the requested page; a normal open keeps the persistent id
        // (and its remembered page/search).
        let settings_id = match initial_page {
            Some(_) => format!(
                "sqlhighland-settings-{}",
                ABOUT_DIALOG_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            None => "sqlhighland-settings".to_string(),
        };
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let hover_bg = cx.theme().accent;
            // Reloaded on every rebuild so the check mark follows the
            // selection while the dialog stays open.
            let current = Preferences::load().theme_name();
            let theme_list_view = view.clone();
            let theme_list = move |_: &RenderOptions, _: &mut Window, _: &mut App| {
                let mut rows = Vec::new();
                for (row_ix, name) in THEME_LIST.into_iter().enumerate() {
                    let selected = name == current;
                    let id = ("settings-theme", row_ix);
                    let row_view = theme_list_view.clone();
                    rows.push(
                        div()
                            .id(id)
                            .w_full()
                            .p_2()
                            .rounded_md()
                            .hover(move |this| this.bg(hover_bg))
                            .on_click(move |_, window, cx| {
                                let view = row_view.clone();
                                view.update(cx, |this, cx| {
                                    let mut prefs = Preferences::load();
                                    prefs.theme = name.to_string();
                                    if let Err(e) = prefs.save() {
                                        this.status =
                                            format!("Preferences save failed: {e:#}").into();
                                    }
                                    crate::guitheme::apply_preferences(&prefs, Some(window), cx);
                                    // Full view re-render: GPUI only repaints
                                    // dirty views, and window refreshes alone
                                    // reuse cached ones. Root too: its background
                                    // paints behind the transparent sidebar/status.
                                    cx.notify();
                                    gpui_kit::component::Root::update(
                                        window,
                                        cx,
                                        |_, _, cx| cx.notify(),
                                    );
                            });
                            window.refresh();
                        })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().flex_1().text_sm().child(name))
                                    .when(selected, |this| {
                                        this.child(
                                            div()
                                                .text_color(muted)
                                                .child(KitIcon::Check),
                                        )
                                    }),
                            )
                            .into_any_element(),
                    );
                }
                v_flex().gap_1().children(rows)
            };
            // Reloaded on every rebuild so switches follow live prefs.
            let current_mode = Preferences::load().completion;
            let show_system = Preferences::load().show_system_schemas;
            let complete_view = view.clone();
            let system_view = view.clone();
            let system_selected = show_system;
            let system_view_outer = system_view.clone();
            dialog
                .title("Settings")
                .w(px(640.))
                .child(
                    div().w_full().h(px(440.)).child(
                        Settings::new(settings_id.clone())
                            .default_selected_index(SelectIndex {
                                page_ix: initial_page.unwrap_or(0),
                                group_ix: None,
                            })
                            .pages(vec![
                            SettingPage::new("Themes")
                                .icon(KitIcon::Palette)
                                .groups(vec![SettingGroup::new().title("Appearance").items(
                                    vec![SettingItem::render(theme_list).keywords([
                                        "theme",
                                        "appearance",
                                        "color",
                                        "dark",
                                        "light",
                                    ])],
                                )]),
                            SettingPage::new("Editor")
                                .icon(KitIcon::SquarePen)
                                .groups(vec![SettingGroup::new()
                                    .title("Suggestions")
                                    .items(vec![
                                        SettingItem::render(move |_, _, _| {
                                            let auto_view = complete_view.clone();
                                            div()
                                                .id("settings-complete-toggle")
                                                .w_full()
                                                .p_2()
                                                .rounded_md()
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .items_center()
                                                        .child(
                                                            v_flex().flex_1()
                                                                .child(
                                                                    div().text_sm().child(
                                                                        "Automatic suggestions",
                                                                    ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(muted)
                                                                        .child(
                                                                            "Popup while typing (off: Ctrl+Space only)",
                                                                        ),
                                                                ),
                                                        )
                                                        .child(
                                                            Switch::new("settings-complete-auto")
                                                                .small()
                                                                .checked(
                                                                    current_mode
                                                                        == CompleteMode::Auto,
                                                                )
                                                                .on_change(
                                                                    move |checked, _, cx| {
                                                                        let apply = if *checked {
                                                                            CompleteMode::Auto
                                                                        } else {
                                                                            CompleteMode::Manual
                                                                        };
                                                                        auto_view
                                                                            .update(
                                                                                cx,
                                                                                |this, cx| {
                                                                                    let mut prefs =
                                                                                        Preferences::load(
                                                                                        );
                                                                                    prefs.completion =
                                                                                        apply;
                                                                                    if let Err(e) =
                                                                                        prefs.save()
                                                                                    {
                                                                                        this.status = format!(
                                                                                            "Preferences save failed: {e:#}"
                                                                                        )
                                                                                        .into();
                                                                                    }
                                                                                    this.complete_auto =
                                                                                        apply
                                                                                            == CompleteMode::Auto;
                                                                                    cx.notify();
                                                                                },
                                                                            );
                                                                    },
                                                                ),
                                                        ),
                                                )
                                        })
                                        .keywords([
                                            "completion",
                                            "suggest",
                                            "automatic",
                                            "manual",
                                            "typing",
                                            "popup",
                                            "shortcut",
                                        ]),
                                        SettingItem::render(move |_, _, _| {
                                            let system_switch_view = system_view_outer.clone();
                                            v_flex().gap_1().child(
                                                div()
                                                    .id("settings-system-schemas")
                                                    .w_full()
                                                    .p_2()
                                                    .rounded_md()
                                                    .child(
                                                        h_flex()
                                                            .gap_2()
                                                            .items_center()
                                                            .child(
        v_flex()
            .flex_1()
                                                                     .child(
                                                                        div()
                                                                            .text_sm()
                                                                            .child(
                                                                                "Show system schemas",
                                                                            ),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(muted)
                                                                            .child(
                                                                                "Include SYS, SYSTEM, XDB, … in suggestions",
                                                                            ),
                                                                    ),
                                                            )
                                                            .child(
                                                                Switch::new("settings-system")
                                                                    .small()
                                                                    .checked(system_selected)
                                                                    .on_change(
                                                                        move |checked, _, cx| {
                                                                            system_switch_view
                                                                                .update(
                                                                                    cx,
                                                                                    |this, cx| {
                                                                                        let mut prefs =
                                                                                            Preferences::load(
                                                                                            );
                                                                                        prefs.show_system_schemas =
                                                                                            *checked;
                                                                                        let _ = prefs
                                                                                            .save(
                                                                                            );
                                                                                        this.show_system =
                                                                                            prefs.show_system_schemas;
                                                                                        // Scope changed: drop caches so
                                                                                        // the next trigger refetches
                                                                                        // with the new filter.
                                                                                        for cache in this.browser.meta
                                                                                            .values(
                                                                                            )
                                                                                        {
                                                                                            if let Ok(mut c) =
                                                                                                cache.lock(
                                                                                                )
                                                                                            {
                                                                                                c.fetched_at =
                                                                                                    None;
                                                                                            }
                                                                                        }
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                        },
                                                                    ),
                                                            ),
                                                    ),
                                            )
                                        })
                                        .keywords([
                                            "system",
                                            "schemas",
                                            "sys",
                                            "hidden",
                                            "filter",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("Font").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            let current = Preferences::load().font_family;
                                            let rows = [
                                                ("", "Theme default"),
                                                ("SF Mono", "SF Mono"),
                                                ("Menlo", "Menlo"),
                                                ("JetBrains Mono", "JetBrains Mono"),
                                                ("Fira Code", "Fira Code"),
                                            ];
                                            v_flex().gap_1().children(rows.into_iter().enumerate().map(
                                                |(ix, (value, label))| {
                                                    let value = value.to_string();
                                                    let ids = [
                                                        "settings-font-default",
                                                        "settings-font-sf",
                                                        "settings-font-menlo",
                                                        "settings-font-jb",
                                                        "settings-font-fira",
                                                    ];
                                                    settings_pick_row(
                                                        ids[ix],
                                                        label.to_string(),
                                                        None,
                                                        current == value,
                                                        cx,
                                                        move |_, window, cx| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.font_family = value.clone();
                                                            Self::save_prefs_status(
                                                                &prefs, cx,
                                                            );
                                                            crate::guitheme::apply_font_prefs(
                                                                &prefs, cx,
                                                            );
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords(["font", "family", "mono", "typeface"]),
                                        SettingItem::render(move |_, _, cx| {
                                            let size = Preferences::load().font_size;
                                            div()
                                                .id("settings-font-size")
                                                .w_full()
                                                .p_2()
                                                .rounded_md()
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .items_center()
                                                        .child(
                                                            v_flex().flex_1()
                                                                .child(
                                                                    div().text_sm().child("Size"),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(
                                                                            cx.theme()
                                                                                .muted_foreground,
                                                                        )
                                                                        .child(
                                                                            "Editor text size in points",
                                                                        ),
                                                                ),
                                                        )
                                                        .child(
                                                            Button::new("settings-font-minus")
                                                                .label("−")
                                                                .small()
                                                                .on_click(
                                                                    move |_,
                                                                          window,
                                                                          cx| {
                                                                        let mut prefs =
                                                                            Preferences::load();
                                                                        prefs.font_size = prefs
                                                                            .font_size
                                                                            .saturating_sub(1)
                                                                            .max(10);
                                                                        Self::save_prefs_status(&prefs, cx);
                                                                        crate::guitheme::apply_font_prefs(
                                                                            &prefs, cx,
                                                                        );
                                                                        window.refresh();
                                                                    },
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .w(px(28.))
                                                                .text_center()
                                                                .child(size.to_string()),
                                                        )
                                                        .child(
                                                            Button::new("settings-font-plus")
                                                                .label("+")
                                                                .small()
                                                                .on_click(
                                                                    move |_,
                                                                          window,
                                                                          cx| {
                                                                        let mut prefs =
                                                                            Preferences::load();
                                                                        prefs.font_size = prefs
                                                                            .font_size
                                                                            .saturating_add(1)
                                                                            .min(24);
                                                                        Self::save_prefs_status(&prefs, cx);
                                                                        crate::guitheme::apply_font_prefs(
                                                                            &prefs, cx,
                                                                        );
                                                                        window.refresh();
                                                                    },
                                                                ),
                                                        ),
                                                )
                                        })
                                        .keywords(["font", "size", "text"]),
                                    ]),
                                ]),
                            SettingPage::new("Results")
                                .icon(KitIcon::Table)
                                .groups(vec![
                                    SettingGroup::new().title("Row limit").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const CAPS: &[(usize, &str)] = &[
                                                (10_000, "10,000"),
                                                (50_000, "50,000"),
                                                (100_000, "100,000"),
                                                (500_000, "500,000"),
                                                (1_000_000, "1,000,000"),
                                            ];
                                            let current =
                                                Preferences::load().result_cap;
                                            let ids = [
                                                "settings-cap-0",
                                                "settings-cap-1",
                                                "settings-cap-2",
                                                "settings-cap-3",
                                                "settings-cap-4",
                                            ];
                                            v_flex().gap_1().children(CAPS.iter().enumerate().map(
                                                |(ix, (n, label))| {
                                                    let n = *n;
                                                    settings_pick_row(
                                                        ids[ix],
                                                        format!("{label} rows"),
                                                        None,
                                                        current == n,
                                                        cx,
                                                        move |_, window, cx| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.result_cap = n;
                                                            Self::save_prefs_status(&prefs, cx);
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords([
                                            "results", "limit", "rows", "cap", "grid",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("Query timeout").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const OPTS: &[(u64, &str)] = &[
                                                (30, "30 seconds"),
                                                (60, "1 minute"),
                                                (120, "2 minutes"),
                                                (300, "5 minutes"),
                                                (0, "Unlimited"),
                                            ];
                                            let current =
                                                Preferences::load().query_timeout_secs;
                                            let ids = [
                                                "settings-timeout-0",
                                                "settings-timeout-1",
                                                "settings-timeout-2",
                                                "settings-timeout-3",
                                                "settings-timeout-4",
                                            ];
                                            v_flex().gap_1().children(OPTS.iter().enumerate().map(
                                                |(ix, (n, label))| {
                                                    let n = *n;
                                                    settings_pick_row(
                                                        ids[ix],
                                                        label.to_string(),
                                                        None,
                                                        current == n,
                                                        cx,
                                                        move |_, window, cx| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.query_timeout_secs = n;
                                                            Self::save_prefs_status(&prefs, cx);
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords([
                                            "query", "timeout", "seconds", "slow",
                                            "cancel", "stuck", "hang",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("CSV export").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const DELIMS: &[(&str, &str)] = &[
                                                (",", "Comma"),
                                                (";", "Semicolon"),
                                                ("\t", "Tab"),
                                                ("|", "Pipe"),
                                            ];
                                            let current =
                                                Preferences::load().csv_delimiter.clone();
                                            let ids = [
                                                "settings-delim-0",
                                                "settings-delim-1",
                                                "settings-delim-2",
                                                "settings-delim-3",
                                            ];
                                            v_flex().gap_1().children(
                                                DELIMS.iter().enumerate().map(
                                                    |(ix, (value, label))| {
                                                        let value = value.to_string();
                                                        settings_pick_row(
                                                            ids[ix],
                                                            label.to_string(),
                                                            None,
                                                            current == value,
                                                            cx,
                                                            move |_, window, cx| {
                                                                let mut prefs =
                                                                    Preferences::load();
                                                                prefs.csv_delimiter =
                                                                    value.clone();
                                                                Self::save_prefs_status(&prefs, cx);
                                                                window.refresh();
                                                            },
                                                        )
                                                    },
                                                ),
                                            )
                                        })
                                        .keywords([
                                            "export", "csv", "delimiter", "separator",
                                        ]),
                                        SettingItem::render(move |_, _, cx| {
                                            v_flex().gap_1().child(
                                                div()
                                                    .id("settings-csv-header")
                                                    .w_full()
                                                    .p_2()
                                                    .rounded_md()
                                                    .child(
                                                        h_flex()
                                                            .gap_2()
                                                            .items_center()
                                                            .child(
                                                                v_flex().flex_1()
                                                                    .child(
                                                                        div().text_sm().child(
                                                                            "Header row",
                                                                        ),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(
                                                                                cx.theme()
                                                                                    .muted_foreground,
                                                                            )
                                                                            .child(
                                                                                "First line holds column names",
                                                                            ),
                                                                    ),
                                                            )
                                                            .child(
                                                                Switch::new("settings-csv-header-sw")
                                                                    .small()
                                                                    .checked(
                                                                        Preferences::load()
                                                                            .csv_header,
                                                                    )
                                                                    .on_change(
                                                                        move |checked, window, cx| {
                                                                            let mut prefs =
                                                                                Preferences::load(
                                                                                );
                                                                            prefs.csv_header =
                                                                                *checked;
                                                                            Self::save_prefs_status(
                                                                                &prefs, cx,
                                                                            );
                                                                            window.refresh();
                                                                        },
                                                                    ),
                                                            ),
                                                    ),
                                            )
                                        })
                                        .keywords(["export", "csv", "header", "columns"]),
                                    ]),
                                ]),
                            SettingPage::new("About")
                                .icon(KitIcon::Info)
                                .groups(vec![SettingGroup::new().title("About").items(vec![
                                    SettingItem::render(move |_, _, _| {
                                        h_flex()
                                            .gap_4()
                                            .items_center()
                                            .child(
                                                img(ABOUT_ICON)
                                                    .w(px(72.))
                                                    .h(px(72.))
                                                    .flex_none()
                                                    .object_fit(ObjectFit::Contain),
                                            )
                                            .child(
                                                v_flex()
                                                    .flex_1()
                                                    .gap_1()
                                                    .child(
                                                        div()
                                                            .text_base()
                                                            .child("SQLHighland"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(muted)
                                                            .child(format!(
                                                                "Version {}",
                                                                env!("CARGO_PKG_VERSION")
                                                            )),
                                                    ),
                                            )
                                    })
                                    .keywords(["about", "version", "name"]),
                                    SettingItem::render(move |_, _, _| {
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .text_xs()
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(muted)
                                                    .child("Author"),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .text_xs()
                                                    .child(env!("CARGO_PKG_AUTHORS")),
                                            )
                                    })
                                    .keywords(["about", "author", "authors", "made by"]),
                                    SettingItem::render(move |_, _, _| {
                                        div()
                                            .w_full()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(env!("CARGO_PKG_DESCRIPTION"))
                                    })
                                    .keywords(["about", "description", "database", "oracle", "driver"]),
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().w_full().gap_1().children(
                                            [
                                                ("Connections", SavedConfig::default_path()),
                                                ("Preferences", Preferences::path()),
                                                (
                                                    "Tabs",
                                                    TabsManifest::manifest_path(),
                                                ),
                                            ]
                                            .into_iter()
                                            .map(|(label, path)| {
                                                div()
                                                    .child(
                                                        div().text_xs().child(label),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(muted)
                                                            .child(
                                                                path.map(|p| {
                                                                    p.to_string_lossy()
                                                                        .into_owned()
                                                                })
                                                                .unwrap_or_else(|_| {
                                                                    "unknown".to_string()
                                                                }),
                                                            ),
                                                    )
                                                    .into_any_element()
                                            })
                                            .collect::<Vec<_>>(),
                                        )
                                    })
                                    .keywords(["about", "paths", "files", "config"]),
                                ])]),
                        ]),
                    ),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("settings-done").label("Done").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }
}

fn settings_pick_row(
    id: impl Into<ElementId>,
    label: String,
    detail: Option<String>,
    selected: bool,
    cx: &App,
    apply: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover_bg = cx.theme().accent;
    let muted = cx.theme().muted_foreground;
    div()
        .id(id)
        .w_full()
        .p_2()
        .rounded_md()
        .hover(move |this| this.bg(hover_bg))
        .on_click(move |ev, window, cx| apply(ev, window, cx))
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .child(div().text_sm().child(label))
                        .children(detail.map(|d| div().text_xs().text_color(muted).child(d))),
                )
                .when(selected, |this| {
                    this.child(div().text_color(muted).child(KitIcon::Check))
                }),
        )
        .into_any_element()
}
