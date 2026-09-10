//! Settings → Permissions. Read-only checks on entry, explicit requests only.
//! These APIs run in the desktop process, never through an agent or engine RPC.

use gpui::{
    AnyElement, Context, IntoElement, Render, SharedString, Task, Window, div, prelude::*, px,
};

use crate::settings::widgets;
use crate::theme::Theme;

#[cfg(target_os = "macos")]
mod macos;

#[derive(Clone, Copy)]
enum Permission {
    Accessibility,
    ScreenRecording,
}

impl Permission {
    const ALL: [Self; 2] = [Self::Accessibility, Self::ScreenRecording];

    fn index(self) -> usize {
        match self {
            Self::Accessibility => 0,
            Self::ScreenRecording => 1,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Accessibility => "Accessibility",
            Self::ScreenRecording => "Screen Recording",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Accessibility => {
                "Allows computer use to inspect and control other apps, including clicking and typing."
            }
            Self::ScreenRecording => {
                "Allows computer use to capture screens and see other apps. This does not grant control of those apps."
            }
        }
    }

    fn settings_url(self) -> &'static str {
        match self {
            Self::Accessibility => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            }
            Self::ScreenRecording => {
                "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
            }
        }
    }

    fn request_label(self) -> &'static str {
        match self {
            Self::Accessibility => "Request Accessibility",
            Self::ScreenRecording => "Request Screen Recording",
        }
    }
}

pub struct PermissionsPage {
    status: Option<[bool; 2]>,
    checking: bool,
    notice: Option<&'static str>,
    error: Option<&'static str>,
    process_scope: SharedString,
    executable_path: Option<SharedString>,
    task: Option<Task<()>>,
}

impl PermissionsPage {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            status: None,
            checking: false,
            notice: None,
            error: None,
            process_scope: format!(
                "These checks apply only to this Crew desktop process (PID {}). They do not report permissions for an agent or a headless daemon, whose macOS identity can differ.",
                std::process::id(),
            )
            .into(),
            executable_path: std::env::current_exe()
                .ok()
                .map(|path| format!("Executable: {}", path.display()).into()),
            task: None,
        };
        page.refresh(cx);
        page
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.read_status(false, cx);
    }

    fn read_status(&mut self, request_screen_recording: bool, cx: &mut Context<Self>) {
        if self.checking || !cfg!(target_os = "macos") {
            return;
        }
        self.checking = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move {
                    #[cfg(target_os = "macos")]
                    {
                        if request_screen_recording {
                            macos::request_screen_recording();
                        }
                        Some(macos::status())
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        let _ = request_screen_recording;
                        None
                    }
                })
                .await;
            this.update(cx, |page, cx| {
                page.status = status;
                page.checking = false;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn request(&mut self, permission: Permission, cx: &mut Context<Self>) {
        if self.checking || !cfg!(target_os = "macos") {
            return;
        }
        self.error = None;
        self.notice = Some(
            "Approve access yourself in macOS. If no prompt appears, use Open System Settings below. After changing access, return here and click Refresh status. macOS may require you to quit and reopen Crew, especially for Screen Recording.",
        );
        match permission {
            Permission::Accessibility => {
                // GPUI click handlers run on the application main thread. AX's
                // prompt is asynchronous, so this does not wait for approval.
                #[cfg(target_os = "macos")]
                if let Err(error) = macos::request_accessibility() {
                    self.error = Some(error);
                }
                self.refresh(cx);
            }
            Permission::ScreenRecording => self.read_status(true, cx),
        }
        cx.notify();
    }

    fn permission_card(
        &self,
        permission: Permission,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let granted = self.status.map(|status| status[permission.index()]);
        let badge = if !cfg!(target_os = "macos") {
            widgets::badge(theme, "Unsupported on this OS").into_any_element()
        } else if self.checking {
            widgets::badge(theme, "Checking…").into_any_element()
        } else if granted == Some(true) {
            widgets::badge_active(theme, "Granted").into_any_element()
        } else {
            widgets::badge(theme, "Not granted").into_any_element()
        };
        let can_request = !self.checking && granted != Some(true);
        let hover_theme = theme.clone();
        let settings_theme = theme.clone();

        widgets::section_card(theme)
            .child(
                widgets::card_row(theme, true).child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap(px(8.0))
                                .child(widgets::row_title(theme, permission.title()))
                                .child(badge),
                        )
                        .child(
                            widgets::page_subtitle(theme, permission.description())
                                .line_height(px(20.0)),
                        )
                        .when(cfg!(target_os = "macos"), |card| {
                            card.child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(8.0))
                                    .child(
                                        widgets::ghost_action(theme)
                                            .id(SharedString::from(format!(
                                                "permission-request-{}",
                                                permission.index()
                                            )))
                                            .when(!can_request, |button| {
                                                button.opacity(0.45).cursor_default()
                                            })
                                            .when(can_request, |button| {
                                                button
                                                    .hover(move |s| {
                                                        widgets::ghost_hover(&hover_theme, s)
                                                    })
                                                    .on_click(cx.listener(move |page, _, _, cx| {
                                                        page.request(permission, cx)
                                                    }))
                                            })
                                            .child(permission.request_label()),
                                    )
                                    .child(
                                        widgets::ghost_action(theme)
                                            .id(SharedString::from(format!(
                                                "permission-settings-{}",
                                                permission.index()
                                            )))
                                            .hover(move |s| {
                                                widgets::ghost_hover(&settings_theme, s)
                                            })
                                            .on_click(cx.listener(move |_, _, _, cx| {
                                                cx.open_url(permission.settings_url())
                                            }))
                                            .child("Open System Settings"),
                                    ),
                            )
                        }),
                ),
            )
            .into_any_element()
    }
}

impl Render for PermissionsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let hover_theme = theme.clone();
        let cards = Permission::ALL.map(|permission| self.permission_card(permission, &theme, cx));

        div()
            .id("permissions-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Permissions", None))
                    .child(
                        widgets::page_subtitle(&theme, "macOS access for computer use on this Mac. Crew cannot approve these permissions for you.")
                            .line_height(px(20.0)),
                    )
                    .child(
                        widgets::page_subtitle(&theme, self.process_scope.clone())
                            .mt(px(16.0))
                            .line_height(px(20.0)),
                    )
                    .when_some(self.executable_path.clone(), |page, path| {
                        page.child(
                            widgets::page_subtitle(&theme, path)
                                .id("permissions-executable")
                                .text_size(px(11.0))
                                .whitespace_nowrap()
                                .overflow_x_scroll(),
                        )
                    })
                    .child(
                        widgets::page_subtitle(&theme, "Remote sessions cannot control this local desktop. Granting access here does not grant it to a remote session or a separately launched daemon.")
                            .line_height(px(20.0)),
                    )
                    .when(cfg!(target_os = "macos"), |page| {
                        page.child(
                            widgets::page_subtitle(&theme, "Not granted can mean access has never been requested, was declined, or is restricted. If macOS lists an older Crew entry, enable the currently installed app in Privacy & Security. Screen Recording may be named Screen & System Audio Recording.")
                                .mt(px(16.0))
                                .line_height(px(20.0)),
                        )
                        .child(
                            div().mt(px(12.0)).flex().child(
                                widgets::ghost_action(&theme)
                                    .id("permissions-refresh")
                                    .when(self.checking, |button| button.opacity(0.45).cursor_default())
                                    .when(!self.checking, |button| {
                                        button
                                            .hover(move |s| widgets::ghost_hover(&hover_theme, s))
                                            .on_click(cx.listener(|page, _, _, cx| page.refresh(cx)))
                                    })
                                    .child(if self.checking { "Checking status…" } else { "Refresh status" }),
                            ),
                        )
                    })
                    .when(!cfg!(target_os = "macos"), |page| {
                        page.child(widgets::warning_strip(&theme, "This page checks macOS permissions only. Permission checks and requests are not supported on this operating system."))
                    })
                    .when_some(self.notice, |page, notice| page.child(widgets::warning_strip(&theme, notice)))
                    .when_some(self.error, |page, error| page.child(widgets::error_strip(&theme, error)))
                    .children(cards),
            )
    }
}
