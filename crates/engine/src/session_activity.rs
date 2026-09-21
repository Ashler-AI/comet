//! Local-controller projection of exact Scaffold rooms into sidebar activity.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
use comet_proto::{
    Session, SessionEnvironmentSource, SessionRef, SessionRoomProjection, SessionStatus,
};
use tokio::task::{AbortHandle, JoinSet};

use crate::{DocHost, WorkspaceHost};

pub(crate) struct SessionActivity(AbortHandle);

impl SessionActivity {
    pub(crate) fn start(host: DocHost, workspace: WorkspaceHost) -> Self {
        let mut refs = workspace.watch_session_refs();
        let task = tokio::spawn(async move {
            // Dropping the set aborts every room observer, including at shutdown.
            let mut tasks = JoinSet::new();
            let mut rooms: HashMap<String, (SessionRoomProjection, AbortHandle)> = HashMap::new();
            loop {
                let wanted: HashMap<_, _> = refs
                    .borrow_and_update()
                    .iter()
                    .filter_map(|reference| {
                        projection(reference, workspace.project_scope())
                            .map(|scope| (reference.chat_id.clone(), scope))
                    })
                    .collect();
                rooms.retain(|id, (scope, task)| {
                    if wanted.get(id) == Some(scope) && !task.is_finished() {
                        true
                    } else {
                        task.abort();
                        false
                    }
                });
                for (id, scope) in wanted {
                    if rooms.contains_key(&id) {
                        continue;
                    }
                    match host.open_projection(&id, Some(&scope)) {
                        Ok(handle) => {
                            let target = workspace.clone();
                            let room = scope.clone();
                            let task = tasks.spawn(async move {
                                let mut messages = handle.watch_messages();
                                let mut previous = None;
                                let mut streamed_at = None;
                                let mut previous_status = None;
                                loop {
                                    let result = {
                                        let tail = messages.borrow_and_update();
                                        handle.doc().collaboration_snapshot().map(|snapshot| {
                                            let Some(agent) = snapshot.sessions.iter().find(|agent| {
                                                agent.session_id == room.session_id && agent.chat_id == room.session_id
                                                    && agent.source == comet_proto::AgentSessionSource::Scaffold
                                            }) else { return Ok(()); };
                                            let latest = tail.entries.iter().rev().find(|entry| {
                                                entry.role == MessageRole::Assistant && match snapshot.message_provenance.iter()
                                                    .find(|provenance| provenance.message_id == entry.id) {
                                                    Some(provenance) => provenance.session_id == agent.session_id,
                                                    None => entry.device_id == agent.owner_device_id
                                                        && snapshot.sessions.iter().filter(|other| other.owner_device_id == agent.owner_device_id).count() == 1,
                                                }
                                            });
                                            // Only newly observed streaming content refreshes old-runtime
                                            // liveness. A snapshot/reconnect or publication-only commit does not.
                                            if previous.as_ref().is_some_and(|old| Some(old) != latest)
                                                && latest.is_some_and(|entry| entry.status == Some(MessageStatus::Streaming)) {
                                                streamed_at = Some(crate::now_ms());
                                            }
                                            if previous.as_ref().map(|entry: &SessionMessageEntry| &entry.id) != latest.map(|entry| &entry.id) {
                                                streamed_at = None;
                                            }
                                            if previous.as_ref() != latest {
                                                previous = latest.cloned();
                                            }
                                            let source_status = agent.status.unwrap_or(SessionStatus::Idle);
                                            let completed = previous_status.is_some_and(|status| {
                                                matches!(status, SessionStatus::Working | SessionStatus::AwaitingInput)
                                                    && matches!(source_status, SessionStatus::Idle | SessionStatus::Errored)
                                            });
                                            previous_status = Some(source_status);
                                            let source_at = agent.updated_at.unwrap_or(agent.created_at);
                                            let mut status = source_status;
                                            let mut updated_at = source_at;
                                            if let Some(entry) = latest
                                                && (status == SessionStatus::Working || entry.created_at > source_at)
                                                && entry.status == Some(MessageStatus::Streaming) {
                                                status = if entry.parts.iter().any(|part| matches!(part, MessagePart::Input { resolved: false, .. })) {
                                                    SessionStatus::AwaitingInput
                                                } else {
                                                    SessionStatus::Working
                                                };
                                                updated_at = updated_at.max(entry.created_at).max(streamed_at.unwrap_or(i64::MIN));
                                            }
                                            let Some(updated_at) = DateTime::<Utc>::from_timestamp_millis(updated_at) else { return Ok(()); };
                                            let last_message_at = tail.entries.iter().map(|entry| entry.created_at)
                                                .chain(completed.then_some(source_at)).max();
                                            target.record_scaffold_activity(&room, &Session {
                                                chat_id: room.session_id.clone(),
                                                device_id: agent.owner_device_id.clone(),
                                                status,
                                                started_at: None,
                                                updated_at,
                                            }, last_message_at)
                                        })
                                    };
                                    match result {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => tracing::warn!(chat = %room.session_id, %error, "workspace activity projection failed"),
                                        Err(error) => tracing::warn!(chat = %room.session_id, %error, "session activity snapshot failed"),
                                    }
                                    if messages.changed().await.is_err() {
                                        break;
                                    }
                                }
                            });
                            rooms.insert(id, (scope, task));
                        }
                        Err(error) => {
                            tracing::warn!(chat = %id, %error, "session activity room open failed")
                        }
                    }
                }
                tokio::select! {
                    changed = refs.changed() => if changed.is_err() { break; },
                    Some(_) = tasks.join_next(), if !tasks.is_empty() => {},
                }
            }
        });
        Self(task.abort_handle())
    }

    pub(crate) fn stop(&self) {
        self.0.abort();
    }
}

impl Drop for SessionActivity {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn projection(reference: &SessionRef, project: &str) -> Option<SessionRoomProjection> {
    let environment = reference.environment.as_ref()?;
    if !matches!(
        environment.source,
        SessionEnvironmentSource::Scaffold { .. }
    ) || environment.scope.project_id != project
        || environment.scope.session_id.as_deref() != Some(reference.chat_id.as_str())
    {
        return None;
    }
    let deployment = environment
        .scope
        .deployment_id
        .as_deref()
        .filter(|id| !id.trim().is_empty())?;
    Some(SessionRoomProjection {
        project_id: project.to_string(),
        deployment_id: deployment.to_string(),
        session_id: reference.chat_id.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{
        AgentSessionRecord, AgentSessionSource, COLLABORATION_SCHEMA_VERSION, ChatIndicator,
        CollaborationScope, HarnessId, PublicationRecord, PublicationValue, ScaffoldLifecycle,
        SessionEnvironment,
    };
    use tokio::sync::watch;

    async fn receive<T: Clone>(rx: &mut watch::Receiver<T>, ready: impl Fn(&T) -> bool) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let value = rx.borrow_and_update().clone();
                if ready(&value) {
                    return value;
                }
                rx.changed().await.unwrap();
            }
        })
        .await
        .expect("activity projection did not arrive")
    }

    #[tokio::test]
    async fn remote_activity_drives_unselected_sidebar_and_reconnect_preserves_seen() {
        let temp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(comet_sync::DocsStore::open(temp.path()).unwrap());
        let workspace = WorkspaceHost::open(
            store.clone(),
            crate::WorkspaceHostConfig {
                device_id: "local".into(),
                device_name: "Test".into(),
                platform: "test".into(),
                project_scope: "project".into(),
                user_id: "owner".into(),
                edge: None,
            },
        )
        .unwrap();
        let host = DocHost::new(
            store,
            crate::DocHostConfig {
                device_id: "local".into(),
                default_harness: HarnessId::Mock,
                edge: None,
            },
        );
        host.set_workspace(workspace.clone());
        let reference = workspace
            .upsert_session_ref(
                "chat",
                Some(SessionEnvironment {
                    source: SessionEnvironmentSource::Scaffold {
                        sandbox_id: "sandbox".into(),
                        region: None,
                        lifecycle: ScaffoldLifecycle::Ready,
                        lifecycle_epoch: Some(1),
                        links: Default::default(),
                    },
                    name: Some("Remote chat".into()),
                    owner_principal: "owner".into(),
                    scope: CollaborationScope {
                        project_id: "project".into(),
                        deployment_id: Some("deployment".into()),
                        session_id: Some("chat".into()),
                        unknown: Default::default(),
                    },
                    source_ref: None,
                    last_activity_at: None,
                    database_environment: None,
                    unknown: Default::default(),
                }),
            )
            .unwrap();
        let room = projection(&reference, "project").unwrap();
        assert!(projection(&reference, "other-project").is_none());
        let mut mismatched = reference.clone();
        mismatched.environment.as_mut().unwrap().scope.session_id = Some("other-chat".into());
        assert!(projection(&mismatched, "project").is_none());
        mismatched.environment.as_mut().unwrap().scope.session_id = Some("chat".into());
        mismatched.environment.as_mut().unwrap().scope.deployment_id = Some(" ".into());
        assert!(projection(&mismatched, "project").is_none());
        let handle = host.open_projection("chat", Some(&room)).unwrap();
        let publish = |id: &str, status, at| {
            handle
                .doc()
                .append_publication(&PublicationRecord {
                    id: format!("{id}/{at}"),
                    schema_version: COLLABORATION_SCHEMA_VERSION,
                    published_at: at,
                    published_by: "owner".into(),
                    value: PublicationValue::AgentSession(Box::new(AgentSessionRecord {
                        session_id: id.into(),
                        chat_id: "chat".into(),
                        owner_subject: "owner".into(),
                        owner_device_id: "remote".into(),
                        source: AgentSessionSource::Scaffold,
                        environment: None,
                        harness: None,
                        model: None,
                        harness_session_id: None,
                        status: Some(status),
                        updated_at: Some(at),
                        created_at: 1_000,
                        unknown: Default::default(),
                    })),
                    unknown: Default::default(),
                })
                .unwrap();
        };
        publish("chat", SessionStatus::Working, 2_000);
        handle
            .doc()
            .push_message(&SessionMessageEntry {
                id: "answer".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "text".into(),
                    text: "Working".into(),
                }],
                created_at: 2_100,
                device_id: "remote".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
                peer_message: None,
            })
            .unwrap();
        let stale_local = Session {
            chat_id: "chat".into(),
            device_id: "local".into(),
            status: SessionStatus::Idle,
            started_at: None,
            updated_at: DateTime::from_timestamp_millis(99_000).unwrap(),
        };
        let (_local_tx, local) = watch::channel(vec![stale_local.clone()]);
        let mut sessions = workspace.merged_sessions_watch(local);
        let mut chats = workspace.watch_chats();
        let bridge = SessionActivity::start(host.clone(), workspace.clone());
        let rows = receive(&mut sessions, |rows| {
            rows.iter().any(|row| row.status == SessionStatus::Working)
        })
        .await;
        let list = receive(&mut chats, |chats| {
            chats.first().is_some_and(|chat| {
                chat.last_message_at
                    .is_some_and(|at| at.timestamp_millis() == 2_100)
            })
        })
        .await;
        let now = DateTime::from_timestamp_millis(5_000).unwrap();
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), now),
            ChatIndicator::Working
        );
        let newer_local = comet_proto::Chat {
            id: "newer-local".into(),
            last_message_at: DateTime::from_timestamp_millis(4_000),
            ..list[0].clone()
        };
        let mut order = vec![
            (ChatIndicator::Working, &list[0]),
            (ChatIndicator::Idle, &newer_local),
        ];
        comet_proto::view::sort_active(&mut order);
        assert_eq!(order[0].1.id, "newer-local");
        workspace.record_session(&stale_local);
        assert_eq!(
            workspace.doc().read_sessions().unwrap()[0].status,
            SessionStatus::Working
        );

        publish("chat", SessionStatus::AwaitingInput, 3_000);
        let rows = receive(&mut sessions, |rows| {
            rows[0].status == SessionStatus::AwaitingInput
        })
        .await;
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), now),
            ChatIndicator::AwaitingInput
        );
        // A completed assistant segment (including text commentary) is not a turn boundary.
        handle
            .doc()
            .set_message_status("answer", MessageStatus::Complete)
            .unwrap();
        publish("other-agent", SessionStatus::Idle, 4_000);
        publish("chat", SessionStatus::Working, 4_100);
        let rows = receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == 4_100
        })
        .await;
        assert_eq!(rows[0].status, SessionStatus::Working);

        publish("chat", SessionStatus::Idle, 5_000);
        let rows = receive(&mut sessions, |rows| rows[0].status == SessionStatus::Idle).await;
        let list = receive(&mut chats, |chats| {
            chats[0]
                .last_message_at
                .is_some_and(|at| at.timestamp_millis() == 5_000)
        })
        .await;
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), now),
            ChatIndicator::Completed
        );
        let mut order = vec![
            (ChatIndicator::Idle, &newer_local),
            (ChatIndicator::Completed, &list[0]),
        ];
        comet_proto::view::sort_active(&mut order);
        assert_eq!(
            order[0].1.id, "chat",
            "remote completion moves above newer local activity"
        );
        workspace.doc().set_chat_seen("chat", now).unwrap();
        workspace.rename_chat("chat", "Keep my title").unwrap();
        drop(bridge);
        let bridge = SessionActivity::start(host.clone(), workspace.clone());
        publish("chat", SessionStatus::Idle, 4_900);
        receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == 4_900
        })
        .await;
        let list = receive(&mut chats, |chats| {
            chats[0].last_seen_at == Some(now) && chats[0].title.as_deref() == Some("Keep my title")
        })
        .await;
        assert_eq!(list[0].last_message_at, Some(now));
        assert_eq!(
            comet_proto::view::display_status(&list[0], sessions.borrow().first(), now),
            ChatIndicator::Idle
        );
        publish("chat", SessionStatus::Idle, 7_000);
        receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == 7_000
        })
        .await;
        assert_eq!(
            workspace
                .doc()
                .chat("chat")
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(now),
            "a redundant idle publication is status, not message activity"
        );

        workspace.remove_session_ref("chat").unwrap();
        receive(&mut chats, Vec::is_empty).await;
        publish("chat", SessionStatus::Working, 6_000);
        // Even a queued observer delivery cannot write after the synchronous unpin.
        workspace
            .record_scaffold_activity(
                &room,
                &Session {
                    status: SessionStatus::Working,
                    updated_at: now,
                    ..rows[0].clone()
                },
                Some(6_000),
            )
            .unwrap();
        assert_eq!(
            workspace
                .doc()
                .chat("chat")
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(now)
        );
        drop(bridge);
    }
}
