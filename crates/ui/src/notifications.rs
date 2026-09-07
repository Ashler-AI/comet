//! Factual session attention, shared by native banners and the existing chimes.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use comet_proto::{Session, SessionStatus};
use gpui::{App, Entity, Global, SystemNotification, SystemNotificationAction};

use crate::settings::UiSettings;
use crate::state::AppState;

const TAG_PREFIX: &str = "crew-session:";

pub struct Preferences {
    pub enabled: bool,
    pub sound: bool,
}
impl Global for Preferences {}

pub fn set_preferences(settings: &UiSettings, cx: &mut App) {
    cx.set_global(Preferences {
        enabled: settings.notifications_enabled,
        sound: settings.sound_enabled,
    });
}

/// GPUI's macOS implementation requires a real bundle. Other supported
/// desktops use the session's XDG notification daemon, which can still deny
/// delivery. GPUI logs those errors but does not expose a delivery receipt.
pub fn unavailable_reason() -> Option<&'static str> {
    #[cfg(target_os = "macos")]
    {
        use objc::runtime::Object;
        use objc::{class, msg_send, sel, sel_impl};
        // app_path() also succeeds for an unbundled command-line process.
        // Match the notification framework's actual bundle-identifier guard.
        let bundled = unsafe {
            let bundle: *mut Object = msg_send![class!(NSBundle), mainBundle];
            let identifier: *mut Object = msg_send![bundle, bundleIdentifier];
            !identifier.is_null()
        };
        if !bundled {
            return Some(
                "Crew notifications require launching the installed Crew app, not a bare executable.",
            );
        }
    }
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        None
    } else {
        Some("Crew native notifications are not supported on this platform.")
    }
}

/// An explicit settings action exercises native delivery without fabricating a
/// session transition or exposing any chat metadata.
pub fn send_test(cx: &App) {
    if !cx.global::<Preferences>().enabled || unavailable_reason().is_some() {
        return;
    }
    cx.show_system_notification(SystemNotification {
        tag: "crew-notification-test".into(),
        title: "Crew".into(),
        body: "Crew desktop alerts are available on this device.".into(),
        actions: Vec::new(),
    });
}

#[derive(Default)]
struct Baseline {
    epoch: u64,
    revision: u64,
    sessions: HashMap<String, (SessionStatus, DateTime<Utc>, u64)>,
}

impl Baseline {
    fn observe(
        &mut self,
        state: &AppState,
        now: DateTime<Utc>,
        mut notify: impl FnMut(&Session, crate::sound::Sound),
    ) {
        if self.revision == state.sessions_revision && self.epoch == state.sessions_epoch {
            return;
        }
        let baseline_only = self.epoch != state.sessions_epoch || self.revision == 0;
        self.epoch = state.sessions_epoch;
        self.revision = state.sessions_revision;
        if baseline_only {
            self.sessions.clear();
        }
        for session in &state.sessions {
            let previous = self.sessions.get_mut(&session.chat_id);
            let Some(previous) = previous else {
                self.sessions.insert(
                    session.chat_id.clone(),
                    (session.status, session.updated_at, self.revision),
                );
                continue;
            };
            previous.2 = self.revision;
            // Replayed and out-of-order rows must not roll the baseline back;
            // otherwise the next heartbeat would look like another transition.
            if session.updated_at <= previous.1 {
                continue;
            }
            let old_status = previous.0;
            *previous = (session.status, session.updated_at, self.revision);
            if baseline_only
                || !state
                    .chats
                    .iter()
                    .any(|chat| chat.id == session.chat_id && !chat.archived)
            {
                continue;
            }
            let Some(sound) = crate::sound::sound_for_session_update(old_status, session, now)
            else {
                continue;
            };
            notify(session, sound);
        }
        self.sessions
            .retain(|_, previous| previous.2 == self.revision);
    }
}

fn show(session: &Session, enabled: bool, viewing: bool, cx: &App) {
    if !enabled || viewing {
        return;
    }
    let body = match session.status {
        SessionStatus::AwaitingInput => "A Crew session needs your input.",
        SessionStatus::Errored => "A Crew session encountered an error.",
        SessionStatus::Idle => "A Crew session finished working.",
        SessionStatus::Working => return,
    };
    cx.show_system_notification(SystemNotification {
        tag: format!("{TAG_PREFIX}{}", session.chat_id).into(),
        title: "Crew".into(),
        body: body.into(),
        actions: vec![SystemNotificationAction {
            id: "open".into(),
            label: "Open Crew session".into(),
        }],
    });
}

/// Application-owned rather than window-owned: closing and reopening the
/// macOS window must neither lose background alerts nor reset the baseline.
pub fn init(state: Entity<AppState>, settings: &UiSettings, cx: &mut App) {
    set_preferences(settings, cx);
    let mut baseline = Baseline::default();
    cx.observe(&state, move |state, cx| {
        let preferences = cx.global::<Preferences>();
        let native = preferences.enabled && unavailable_reason().is_none();
        let sound_enabled = preferences.sound;
        baseline.observe(state.read(cx), Utc::now(), |session, sound| {
            let viewing = cx
                .active_window()
                .and_then(|window| window.downcast::<crate::shell::Shell>())
                .and_then(|window| window.read(cx).ok())
                .is_some_and(|shell| shell.is_viewing_session(&session.chat_id, cx));
            let show_native = native && !viewing;
            show(session, native, viewing, cx);
            // GPUI posts silent macOS banners. XDG daemons may add sound,
            // so never layer a Crew chime over a Linux banner.
            if sound_enabled && !(show_native && cfg!(target_os = "linux")) {
                crate::sound::play(sound);
            }
        });
    })
    .detach();
    cx.on_system_notification_response(move |response, cx| {
        if response.action_id.as_deref().is_some_and(|id| id != "open") {
            return;
        }
        let Some(chat_id) = response.tag.strip_prefix(TAG_PREFIX) else {
            return;
        };
        if !state
            .read(cx)
            .chats
            .iter()
            .any(|chat| chat.id == chat_id && !chat.archived)
        {
            return;
        }
        cx.dismiss_system_notification(&response.tag);
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<crate::ReopenState>()
        {
            let boot = reopen.boot.clone();
            crate::open_main_window(state.clone(), boot, cx);
        }
        for window in cx.windows() {
            if let Some(window) = window.downcast::<crate::shell::Shell>() {
                let _ = window.update(cx, |shell, window, cx| {
                    shell.open_notified_session(chat_id.to_owned(), window, cx);
                });
                break;
            }
        }
        cx.activate(true);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionStatus::*;

    fn state(now: DateTime<Utc>) -> AppState {
        let mut state = AppState::new();
        state.chats.push(
            serde_json::from_value(serde_json::json!({
                "id": "chat", "deviceId": "device", "archived": false, "createdAt": now
            }))
            .unwrap(),
        );
        state.sessions_epoch = 1;
        state
    }

    fn publish(
        baseline: &mut Baseline,
        state: &mut AppState,
        status: SessionStatus,
        updated_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Vec<SessionStatus> {
        state.apply_sessions(vec![Session {
            chat_id: "chat".into(),
            device_id: "device".into(),
            status,
            started_at: None,
            updated_at,
        }]);
        let mut alerts = Vec::new();
        baseline.observe(state, now, |session, _| alerts.push(session.status));
        alerts
    }

    #[test]
    fn initial_and_reconnected_snapshots_are_silent() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = state(now);
        let mut baseline = Baseline::default();
        assert!(publish(&mut baseline, &mut state, AwaitingInput, now, now).is_empty());
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Working,
                now + chrono::Duration::seconds(1),
                now
            )
            .is_empty()
        );
        state.sessions_epoch += 1;
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Idle,
                now + chrono::Duration::seconds(2),
                now
            )
            .is_empty()
        );
        assert_eq!(
            publish(
                &mut baseline,
                &mut state,
                Errored,
                now + chrono::Duration::seconds(3),
                now
            ),
            vec![Errored]
        );
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Errored,
                now + chrono::Duration::seconds(4),
                now
            )
            .is_empty()
        );
    }

    #[test]
    fn out_of_order_replay_cannot_create_a_second_completion() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = state(now);
        let mut baseline = Baseline::default();
        assert!(publish(&mut baseline, &mut state, Working, now, now).is_empty());
        assert_eq!(
            publish(
                &mut baseline,
                &mut state,
                Idle,
                now + chrono::Duration::seconds(2),
                now
            ),
            vec![Idle]
        );
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Working,
                now + chrono::Duration::seconds(1),
                now
            )
            .is_empty()
        );
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Idle,
                now + chrono::Duration::seconds(3),
                now
            )
            .is_empty()
        );
        assert_eq!(
            publish(
                &mut baseline,
                &mut state,
                AwaitingInput,
                now + chrono::Duration::seconds(4),
                now
            ),
            vec![AwaitingInput]
        );
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Errored,
                now + chrono::Duration::seconds(4),
                now
            )
            .is_empty()
        );
    }

    #[test]
    fn stale_and_archived_updates_never_alert_or_reappear_as_transitions() {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = state(now);
        let mut baseline = Baseline::default();
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Working,
                now - chrono::Duration::seconds(50),
                now
            )
            .is_empty()
        );
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Idle,
                now - chrono::Duration::seconds(46),
                now
            )
            .is_empty()
        );
        assert!(publish(&mut baseline, &mut state, Idle, now, now).is_empty());
        state.chats[0].archived = true;
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Errored,
                now + chrono::Duration::seconds(1),
                now
            )
            .is_empty()
        );
        state.chats[0].archived = false;
        assert!(
            publish(
                &mut baseline,
                &mut state,
                Errored,
                now + chrono::Duration::seconds(2),
                now
            )
            .is_empty()
        );
        state.apply_sessions(Vec::new());
        baseline.observe(&state, now, |_, _| panic!("removed row alerted"));
        assert!(
            publish(
                &mut baseline,
                &mut state,
                AwaitingInput,
                now + chrono::Duration::seconds(3),
                now
            )
            .is_empty()
        );
        baseline.observe(&state, now + chrono::Duration::seconds(60), |_, _| {
            panic!("display expiry alerted")
        });
    }

    #[gpui::test]
    fn native_banner_requires_opt_in_and_an_unviewed_session(cx: &mut gpui::TestAppContext) {
        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let session = Session {
            chat_id: "chat".into(),
            device_id: "device".into(),
            status: AwaitingInput,
            started_at: None,
            updated_at: now,
        };
        cx.update(|cx| {
            cx.set_app_identity("comet", "Crew");
            show(&session, false, false, cx);
            show(&session, true, true, cx);
        });
        assert!(cx.shown_system_notifications().is_empty());
        cx.update(|cx| show(&session, true, false, cx));
        let delivered = cx.delivered_system_notifications();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].tag.as_ref(), "crew-session:chat");
        assert_eq!(delivered[0].actions[0].id.as_ref(), "open");
    }

    #[gpui::test]
    fn native_click_reopens_session_and_rejects_stale_targets(cx: &mut gpui::TestAppContext) {
        use gpui::{AppContext as _, SystemNotificationResponse};

        let now = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|_| {
            let mut state = state(now);
            let mut other = state.chats[0].clone();
            other.id = "other".into();
            other.archived = true;
            state.chats.push(other);
            state
        });
        let boot = crate::state::EngineBootConfig {
            data_dir: dir.path().to_path_buf(),
            ipc_port: 0,
            edge_url: "http://127.0.0.1:1".into(),
            edge_token: None,
            project_scope: "notification-test".into(),
            deployment_id: None,
            scaffold_url: None,
            default_harness: comet_proto::HarnessId::Omp,
            runtime_profile: comet_proto::RuntimeProfile::LocalController,
        };
        cx.update(|cx| {
            crate::theme::Theme::install(crate::theme::Appearance::Dark, cx);
            init(state.clone(), &UiSettings::default(), cx);
            cx.set_global(crate::ReopenState {
                state: state.clone(),
                boot,
            });
        });
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "crew-session:missing".into(),
            action_id: None,
        });
        cx.update(|cx| assert!(cx.windows().is_empty()));

        // A body click recreates the closed main window around the existing
        // state, rather than bootstrapping a new engine or losing selection.
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "crew-session:chat".into(),
            action_id: None,
        });
        cx.update(|cx| {
            assert_eq!(state.read(cx).selected_chat.as_deref(), Some("chat"));
            let windows = cx.windows();
            assert_eq!(windows.len(), 1);
            let window = windows[0].downcast::<crate::shell::Shell>().unwrap();
            assert!(window.read(cx).unwrap().is_viewing_session("chat", cx));
        });

        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "crew-session:other".into(),
            action_id: Some("open".into()),
        });
        cx.update(|cx| assert_eq!(state.read(cx).selected_chat.as_deref(), Some("chat")));
        state.update(cx, |state, _| state.chats[1].archived = false);
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "crew-session:other".into(),
            action_id: Some("dismiss".into()),
        });
        cx.update(|cx| assert_eq!(state.read(cx).selected_chat.as_deref(), Some("chat")));

        // The action button navigates the existing window, without opening
        // another window or treating an opaque session id as a URL.
        cx.simulate_system_notification_response(SystemNotificationResponse {
            tag: "crew-session:other".into(),
            action_id: Some("open".into()),
        });
        cx.update(|cx| {
            assert_eq!(state.read(cx).selected_chat.as_deref(), Some("other"));
            let windows = cx.windows();
            assert_eq!(windows.len(), 1);
            let window = windows[0].downcast::<crate::shell::Shell>().unwrap();
            assert!(window.read(cx).unwrap().is_viewing_session("other", cx));
        });
        assert_eq!(
            cx.dismissed_system_notifications(),
            vec![gpui::SharedString::from("crew-session:chat"), "crew-session:other".into()]
        );
    }
}
