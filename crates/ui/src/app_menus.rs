//! Native application menus built from GPUI's public menu and action APIs.
//!
//! The module owns only process/window commands and delegates text editing to
//! the focused GPUI responder through `OsAction`. Product routes and session
//! actions stay in the shell command palette.

use gpui::{App, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType, Window, actions};

use crate::appearance::{self, AppearanceMode};
use crate::composer;
use crate::shell;

actions!(
    ashler_comet,
    [
        About,
        Quit,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        CloseWindow,
        AppearanceSystem,
        AppearanceLight,
        AppearanceDark,
    ]
);

/// Register process-wide handlers before installing menus.
pub fn init(cx: &mut App) {
    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.on_action(|_: &Minimize, cx| update_active_window(cx, |window| window.minimize_window()));
    cx.on_action(|_: &Zoom, cx| update_active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &CloseWindow, cx| update_active_window(cx, Window::remove_window));
    cx.on_action(|_: &AppearanceSystem, cx| appearance::set_mode(AppearanceMode::System, cx));
    cx.on_action(|_: &AppearanceLight, cx| appearance::set_mode(AppearanceMode::Light, cx));
    cx.on_action(|_: &AppearanceDark, cx| appearance::set_mode(AppearanceMode::Dark, cx));
}

fn update_active_window(cx: &mut App, command: fn(&mut Window)) {
    let Some(window) = cx.active_window() else {
        return;
    };
    window.update(cx, |_, window, _| command(window)).ok();
}

/// Install fixed native key equivalents after the customizable keymap resets.
pub fn bind_keys(cx: &mut App) {
    if cfg!(target_os = "macos") {
        cx.bind_keys(macos_key_bindings());
    }
}

fn macos_key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("alt-cmd-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-shift-w", CloseWindow, None),
    ]
}

fn application_menu(macos: bool) -> Menu {
    let mut items = vec![
        MenuItem::action("About Crew", About).disabled(true),
        MenuItem::separator(),
    ];
    if macos {
        items.extend([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Hide Crew", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
        ]);
    }
    items.push(MenuItem::action("Quit Crew", Quit));
    Menu::new("Crew").items(items)
}

fn file_menu() -> Menu {
    Menu::new("File").items([
        MenuItem::action("New Session", shell::NewSession),
        MenuItem::separator(),
        MenuItem::action("Close Session", shell::CloseSession),
    ])
}

fn edit_menu() -> Menu {
    Menu::new("Edit").items([
        MenuItem::action("Undo", composer::Undo),
        MenuItem::action("Redo", composer::Redo),
        MenuItem::separator(),
        MenuItem::os_action("Cut", composer::Cut, OsAction::Cut),
        MenuItem::os_action("Copy", composer::Copy, OsAction::Copy),
        MenuItem::os_action("Paste", composer::Paste, OsAction::Paste),
        MenuItem::separator(),
        MenuItem::os_action("Select All", composer::SelectAll, OsAction::SelectAll),
    ])
}

fn view_menu() -> Menu {
    Menu::new("View").items([
        MenuItem::action("Appearance: System", AppearanceSystem),
        MenuItem::action("Appearance: Light", AppearanceLight),
        MenuItem::action("Appearance: Dark", AppearanceDark),
    ])
}

fn window_menu() -> Menu {
    Menu::new("Window").items([
        MenuItem::action("Minimize", Minimize),
        MenuItem::action("Zoom", Zoom),
        MenuItem::separator(),
        MenuItem::action("Close Window", CloseWindow),
    ])
}

/// Build the native menu bar. Platform-only commands are omitted where the
/// host operating system does not provide matching application behavior.
pub fn app_menus() -> Vec<Menu> {
    let macos = cfg!(target_os = "macos");
    let mut menus = vec![
        application_menu(macos),
        file_menu(),
        edit_menu(),
        view_menu(),
    ];
    if macos {
        menus.push(window_menu());
    }
    menus
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Action as _;

    #[test]
    fn product_menu_uses_crew_name_and_quit_action() {
        let menus = app_menus();
        assert_eq!(menus[0].name.as_ref(), "Crew");
        let MenuItem::Action { name, action, .. } = menus[0].items.last().unwrap() else {
            panic!("last product menu item should quit");
        };
        assert_eq!(name.as_ref(), "Quit Crew");
        assert_eq!(action.name(), Quit.name());
    }

    #[test]
    fn edit_menu_exposes_native_clipboard_commands() {
        let menu = edit_menu();
        let actions = menu
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { action, .. } => Some(action.name()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(actions.contains(&composer::Cut.name()));
        assert!(actions.contains(&composer::Copy.name()));
        assert!(actions.contains(&composer::Paste.name()));
        assert!(actions.contains(&composer::SelectAll.name()));
    }

    #[test]
    fn macos_shortcut_table_covers_process_and_window_commands() {
        assert_eq!(macos_key_bindings().len(), 5);
    }
}
