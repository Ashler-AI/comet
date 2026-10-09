//! Project owner-published session-room status into sidebar activity.
//! Room observers follow membership identity as well as physical scope: an
//! imported membership settlement must reproject quiet owner state after the
//! workspace fences the previous observation. Metadata never refreshes its clock.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use comet_proto::{
    Chat, Session, SessionEnvironmentSource, SessionRef, SessionRoomProjection, SessionStatus,
};
use tokio::task::{AbortHandle, JoinSet};

use crate::{DocHost, WorkspaceHost};

/// One public thread's activity, without letting an idle sibling hide a live writer.
pub(crate) fn aggregate_owner_thread(
    records: &[comet_proto::AgentSessionRecord],
    canonical: &comet_proto::AgentSessionRecord,
    now: i64,
) -> Option<Session> {
    let writer = comet_proto::view::owner_thread_activity(records, canonical, now)?;
    let updated_at = DateTime::<Utc>::from_timestamp_millis(writer.updated_at.unwrap_or(writer.created_at))?;
    Some(Session {
        chat_id: canonical.chat_id.clone(), device_id: canonical.owner_device_id.clone(),
        status: writer.status?, started_at: writer.started_at.and_then(DateTime::<Utc>::from_timestamp_millis), updated_at,
        model_retry: if writer.status == Some(SessionStatus::Working) { writer.model_retry } else { None },
    })
}

fn projected_rooms<'a>(
    refs: &'a [SessionRef],
    chats: &[Chat],
    sessions: &[Session],
    project: &str,
    local_device: &str,
    now: i64,
) -> HashMap<String, (&'a SessionRef, Option<SessionRoomProjection>)> {
    const RECENT_ROOMS: usize = 64;
    let chats: HashMap<_, _> = chats.iter().map(|chat| (chat.id.as_str(), chat)).collect();
    let active: std::collections::HashSet<_> = sessions.iter().filter(|session| {
        matches!(session.status, SessionStatus::Working | SessionStatus::AwaitingInput)
            && now.saturating_sub(session.updated_at.timestamp_millis()) <= comet_proto::view::SESSION_STALE_MS
    }).map(|session| session.chat_id.as_str()).collect();
    let mut candidates: Vec<_> = refs.iter().filter_map(|reference| {
        let chat = chats.get(reference.chat_id.as_str());
        if chat.is_some_and(|chat| chat.archived) { return None; }
        let scaffold = reference.environment.as_ref().is_some_and(|environment| {
            matches!(environment.source, SessionEnvironmentSource::Scaffold { .. })
        });
        // Exact-id imports initially have only membership. Observe their owner
        // publication too; a workspace host row may arrive later or never.
        if !scaffold && chat.is_some_and(|chat| chat.device_id == local_device) { return None; }
        let recency = chat.and_then(|chat| chat.last_message_at).unwrap_or(reference.added_at);
        Some((reference, recency))
    }).collect();
    candidates.sort_unstable_by(|(a, at), (b, bt)| bt.cmp(at).then_with(|| a.chat_id.cmp(&b.chat_id)));
    candidates.into_iter().enumerate().filter(|(index, (reference, _))| {
        *index < RECENT_ROOMS || active.contains(reference.chat_id.as_str())
    }).filter_map(|(_, (reference, _))| {
        room_projection(reference, project).map(|scope| (reference.chat_id.clone(), (reference, scope)))
    }).collect()
}

pub(crate) struct SessionActivity(AbortHandle);

impl SessionActivity {
    pub(crate) fn start(host: DocHost, workspace: WorkspaceHost) -> Self {
        let mut refs = workspace.watch_session_refs();
        let mut chats = workspace.watch_chats();
        let mut sessions = workspace.watch_session_rows();
        let task = tokio::spawn(async move {
            // Dropping the set aborts every room observer, including at shutdown.
            let mut tasks = JoinSet::new();
            // Cold history lives in the owner-published workspace index, not 1,600
            // pinned documents. Fresh active rows wake their exact session rooms.
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            let mut rooms: HashMap<String, (SessionRef, Option<SessionRoomProjection>, AbortHandle)> = HashMap::new();
            loop {
                {
                    let chat_rows = chats.borrow_and_update();
                    let ref_rows = refs.borrow_and_update();
                    let session_rows = sessions.borrow_and_update();
                    let wanted = projected_rooms(&ref_rows, &chat_rows, &session_rows, workspace.project_scope(), workspace.device_id(), crate::now_ms());
                // Workspace imports can settle membership without changing the
                // physical room. Re-observe that identity after publish fences
                // its previous projection, even when room content stays quiet.
                rooms.retain(|id, (observed, scope, task)| {
                    if wanted.get(id).is_some_and(|(reference, wanted_scope)| {
                        reference.added_at == observed.added_at && reference.environment == observed.environment
                            && wanted_scope == scope
                    }) && !task.is_finished() {
                        true
                    } else {
                        task.abort();
                        false
                    }
                });
                for (id, (reference, scope)) in wanted {
                    if rooms.contains_key(&id) {
                        continue;
                    }
                    match host.open_projection(&id, scope.as_ref()) {
                        Ok(handle) => {
                            let target = workspace.clone();
                            let room = scope.clone();
                            let chat_id = id.clone();
                            let task = tasks.spawn(async move {
                                let mut messages = handle.watch_messages();
                                let mut previous_status = None;
                                loop {
                                    let result = {
                                        let tail = messages.borrow_and_update();
                                        handle.doc().collaboration_snapshot().map(|snapshot| {
                                            let Some(agent) = snapshot.sessions.iter().find(|agent| {
                                                agent.session_id == chat_id && agent.chat_id == chat_id
                                                    && !agent.owner_device_id.is_empty()
                                                    && agent.source == if room.is_some() {
                                                        comet_proto::AgentSessionSource::Scaffold
                                                    } else {
                                                        comet_proto::AgentSessionSource::Local
                                                    }
                                            }) else { return Ok(()); };
                                            // Transcript segments are not turn boundaries or liveness evidence.
                                            // Only the owning runtime may publish working, idle, or input state.
                                            let Some(activity) = aggregate_owner_thread(&snapshot.sessions, agent, crate::now_ms()) else { return Ok(()); };
                                            let status = activity.status;
                                            let source_at = activity.updated_at.timestamp_millis();
                                            let completed = previous_status.is_some_and(|old| {
                                                matches!(old, SessionStatus::Working | SessionStatus::AwaitingInput)
                                                    && matches!(status, SessionStatus::Idle | SessionStatus::Errored)
                                            });
                                            previous_status = Some(status);

                                            let last_message_at = tail.entries.iter().map(|entry| entry.created_at)
                                                .chain(completed.then_some(source_at)).max();
                                            target.record_session_activity(room.as_ref(), &agent.owner_subject, &activity, last_message_at)
                                        })
                                    };
                                    match result {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => tracing::warn!(chat = %chat_id, %error, "workspace activity projection failed"),
                                        Err(error) => tracing::warn!(chat = %chat_id, %error, "session activity snapshot failed"),
                                    }
                                    if messages.changed().await.is_err() {
                                        break;
                                    }
                                }
                            });
                            rooms.insert(id, (reference.clone(), scope, task));
                        }
                        Err(error) => {
                            tracing::warn!(chat = %id, %error, "session activity room open failed")
                        }
                    }
                }
                }
                tokio::select! {
                    changed = refs.changed() => if changed.is_err() { break; },
                    changed = chats.changed() => if changed.is_err() { break; },
                    changed = sessions.changed() => if changed.is_err() { break; },
                    _ = tick.tick() => {},
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

/// `None` means an invalid explicit route, not permission to join the default room.
pub(crate) fn room_projection(reference: &SessionRef, project: &str) -> Option<Option<SessionRoomProjection>> {
    if reference.environment.as_ref().is_some_and(|environment| {
        matches!(environment.source, SessionEnvironmentSource::Scaffold { .. })
    }) {
        projection(reference, project).map(Some)
    } else {
        Some(None)
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
    use comet_doc::{MessagePart, MessageRole, MessageStatus, SessionMessageEntry};
    use comet_proto::{
        AgentSessionRecord, AgentSessionSource, COLLABORATION_SCHEMA_VERSION, ChatIndicator,
        CollaborationScope, HarnessId, PublicationRecord, PublicationValue, ScaffoldLifecycle,
        SessionEnvironment,
    };
    use tokio::sync::watch;

    #[test]
    fn owned_children_drive_thread_activity_without_relocation_or_idle_masking() {
        let canonical = AgentSessionRecord {
            session_id: "chat".into(), chat_id: "chat".into(), owner_subject: "owner".into(),
            owner_device_id: "comet-scaffold-sandbox-e2".into(), source: AgentSessionSource::Scaffold,
            environment: Some(SessionEnvironment {
                source: SessionEnvironmentSource::Scaffold { sandbox_id: "sandbox".into(), region: None,
                    lifecycle: ScaffoldLifecycle::Ready, lifecycle_epoch: Some(2), links: Default::default() },
                name: None, owner_principal: "owner".into(),
                scope: CollaborationScope { project_id: "project".into(), deployment_id: Some("deployment".into()),
                    session_id: Some("chat".into()), unknown: Default::default() },
                source_ref: None, last_activity_at: None, database_environment: None, unknown: Default::default(),
            }),
            harness: None, model: None, harness_session_id: None,
            model_retry: None,
            started_at: None,
            status: Some(SessionStatus::Idle), updated_at: Some(100_100), created_at: 1,
            unknown: Default::default(),
        };
        let mut child = canonical.clone();
        child.session_id = "child".into();
        child.environment.as_mut().unwrap().scope.session_id = Some("child".into());
        child.status = Some(SessionStatus::Working);
        child.updated_at = Some(100_000);
        child.started_at = Some(90_000);
        let mut unrelated = child.clone();
        unrelated.session_id = "foreign".into();
        unrelated.owner_subject = "foreign-owner".into();
        unrelated.status = Some(SessionStatus::AwaitingInput);
        unrelated.updated_at = Some(100_200);
        let mut records = vec![canonical.clone(), child.clone(), unrelated];
        let aggregate = aggregate_owner_thread(&records, &canonical, 100_200).unwrap();
        assert_eq!(aggregate.chat_id, "chat");
        assert_eq!(aggregate.device_id, canonical.owner_device_id);
        assert_eq!(aggregate.status, SessionStatus::Working);
        assert_eq!(aggregate.started_at.unwrap().timestamp_millis(), 90_000);
        assert_eq!(aggregate.updated_at.timestamp_millis(), 100_000, "idle siblings cannot refresh active liveness");
        let stale = aggregate_owner_thread(&records, &canonical, 145_001).unwrap();
        assert_eq!(comet_proto::view::effective_indicator(Some(&stale), DateTime::from_timestamp_millis(145_001).unwrap()), comet_proto::view::Indicator::Unreachable);
        records[1].status = Some(SessionStatus::AwaitingInput);
        records[1].updated_at = Some(100_300);
        let mut working_sibling = child.clone();
        working_sibling.session_id = "working-sibling".into();
        working_sibling.updated_at = Some(100_350);
        records.push(working_sibling);
        assert_eq!(aggregate_owner_thread(&records, &canonical, 100_350).unwrap().status, SessionStatus::AwaitingInput);
        records.pop();
        // Same subject does not make an old host lifecycle or another deployment current.
        let mut obsolete = child.clone();
        obsolete.owner_device_id = "comet-scaffold-sandbox-e1".into();
        records[1] = obsolete;
        assert_eq!(aggregate_owner_thread(&records, &canonical, 100_300).unwrap().status, SessionStatus::Idle);
        let mut previous_epoch = child.clone();
        if let SessionEnvironmentSource::Scaffold { lifecycle_epoch, .. } = &mut previous_epoch.environment.as_mut().unwrap().source {
            *lifecycle_epoch = Some(1);
        }
        records[1] = previous_epoch;
        assert_eq!(aggregate_owner_thread(&records, &canonical, 100_300).unwrap().status, SessionStatus::Idle);
        let mut misplaced = child.clone();
        misplaced.environment.as_mut().unwrap().scope.deployment_id = Some("other".into());
        records[1] = misplaced;
        assert_eq!(aggregate_owner_thread(&records, &canonical, 100_300).unwrap().status, SessionStatus::Idle);
        records[1] = child;
        records[1].status = Some(SessionStatus::Idle);
        records[1].updated_at = Some(100_301);
        let complete = aggregate_owner_thread(&records, &canonical, 100_301).unwrap();
        assert_eq!(complete.status, SessionStatus::Idle);
        assert_eq!(complete.updated_at.timestamp_millis(), 100_301);
        let mut local = canonical.clone();
        local.source = AgentSessionSource::Local;
        local.environment = None;
        local.owner_device_id = "devbox".into();
        let mut local_child = local.clone();
        local_child.session_id = "local-child".into();
        local_child.status = Some(SessionStatus::Working);
        assert_eq!(aggregate_owner_thread(&[local.clone(), local_child], &local, 100_301).unwrap().status, SessionStatus::Working);
    }

    #[test]
    fn dormant_index_does_not_pin_rooms_and_cold_scaffold_activity_wakes() {
        let refs: Vec<_> = (0..1_600).map(|i| {
            let id = format!("chat-{i:04}");
            SessionRef {
                chat_id: id.clone(), added_at: DateTime::from_timestamp_millis(i).unwrap(), startup: None,
                environment: Some(SessionEnvironment {
                    source: SessionEnvironmentSource::Scaffold {
                        sandbox_id: "sandbox".into(), region: None, lifecycle: ScaffoldLifecycle::Ready,
                        lifecycle_epoch: Some(1), links: Default::default(),
                    },
                    name: None, owner_principal: "owner".into(),
                    scope: CollaborationScope {
                        project_id: "project".into(), deployment_id: Some("deployment".into()),
                        session_id: Some(id), unknown: Default::default(),
                    },
                    source_ref: None, last_activity_at: None, database_environment: None, unknown: Default::default(),
                }),
            }
        }).collect();
        let dormant = projected_rooms(&refs, &[], &[], "project", "local", 100_000);
        assert_eq!(dormant.len(), 64);
        assert!(!dormant.contains_key("chat-0000"));
        let mut active = Session {
            chat_id: "chat-0000".into(), device_id: "comet-scaffold-sandbox-e1".into(),
            status: SessionStatus::Working, started_at: None,
            model_retry: None,
            updated_at: DateTime::from_timestamp_millis(100_000).unwrap(),
        };
        let awake = projected_rooms(&refs, &[], std::slice::from_ref(&active), "project", "local", 100_000);
        assert_eq!(awake.len(), 65);
        assert_eq!(awake.get("chat-0000").map(|(_, scope)| scope), Some(&projection(&refs[0], "project")));
        active.status = SessionStatus::AwaitingInput;
        assert!(projected_rooms(&refs, &[], std::slice::from_ref(&active), "project", "local", 145_000).contains_key("chat-0000"));
        assert!(!projected_rooms(&refs, &[], std::slice::from_ref(&active), "project", "local", 145_001).contains_key("chat-0000"));
        assert_eq!(comet_proto::view::effective_indicator(Some(&active), DateTime::from_timestamp_millis(145_001).unwrap()), comet_proto::view::Indicator::Unreachable);
    }

    async fn receive<T: Clone + std::fmt::Debug>(rx: &mut watch::Receiver<T>, ready: impl Fn(&T) -> bool) -> T {
        receive_for(rx, "activity projection", ready).await
    }

    async fn receive_for<T: Clone + std::fmt::Debug>(
        rx: &mut watch::Receiver<T>,
        stage: &str,
        ready: impl Fn(&T) -> bool,
    ) -> T {
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
        .unwrap_or_else(|_| panic!("{stage} did not arrive; last observed value: {:?}", *rx.borrow()))
    }

    #[tokio::test]
    async fn ordinary_membership_hydrates_unselected_status_without_claiming_a_chat() {
        const CHAT: &str = "00000000-0000-4000-8000-000000000019";
        let temp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(comet_sync::DocsStore::open(temp.path()).unwrap());
        let workspace = WorkspaceHost::open(store.clone(), crate::WorkspaceHostConfig {
            device_id: "local".into(), device_name: "Local".into(), platform: "test".into(),
            project_scope: "project".into(), user_id: "owner".into(), edge: None,
        }).unwrap();
        // This is exactly AddSessionRef's ordinary UUID write: no chat placement.
        workspace.upsert_session_ref(CHAT, None).unwrap();
        let at = crate::now_ms();
        let publication = |status, at| PublicationRecord {
            id: format!("owner/{at}"), schema_version: COLLABORATION_SCHEMA_VERSION,
            published_at: at, published_by: "owner".into(),
            value: PublicationValue::AgentSession(Box::new(AgentSessionRecord {
                session_id: CHAT.into(), chat_id: CHAT.into(), owner_subject: "owner".into(),
                owner_device_id: "remote".into(), source: AgentSessionSource::Local,
                environment: None, harness: None, model: None, harness_session_id: None,
                model_retry: None,
                started_at: None,
                status: Some(status), updated_at: Some(at), created_at: at,
                unknown: Default::default(),
            })), unknown: Default::default(),
        };
        let source = comet_doc::SessionDoc::init(CHAT).unwrap();
        source.append_publication(&publication(SessionStatus::Working, at)).unwrap();
        store.save_snapshot(CHAT, &source.export_snapshot().unwrap()).unwrap();
        let host = DocHost::new(store, crate::DocHostConfig {
            device_id: "local".into(), default_harness: HarnessId::Mock, edge: None,
        });
        host.set_workspace(workspace.clone());
        let (_local, local) = watch::channel(Vec::new());
        let mut sessions = workspace.merged_sessions_watch(local);
        let bridge = SessionActivity::start(host.clone(), workspace.clone());
        receive_for(&mut sessions, "rowless owner working", |rows| rows.iter().any(|row| {
            row.chat_id == CHAT && row.device_id == "remote" && row.status == SessionStatus::Working
        })).await;
        let room = host.open(CHAT).unwrap();
        room.doc().append_publication(&publication(SessionStatus::Idle, at + 1)).unwrap();
        let rows = receive_for(&mut sessions, "rowless owner completed", |rows| rows.iter().any(|row| {
            row.chat_id == CHAT && row.status == SessionStatus::Idle
        })).await;
        assert!(workspace.doc().chat(CHAT).unwrap().is_none(), "imports must retain shared-session routing");
        assert!(!workspace.is_host(CHAT), "membership cannot claim execution ownership");
        let current = rows.iter().find(|row| row.chat_id == CHAT).unwrap();
        let before = workspace.doc().read_sessions().unwrap();
        workspace.record_session_activity(None, "other-principal", &Session {
            status: SessionStatus::Working, ..current.clone()
        }, None).unwrap();
        assert_eq!(workspace.doc().read_sessions().unwrap(), before);

        // A later workspace host row is authoritative; an old canonical owner
        // cannot relocate it or publish over the newly recorded device.
        workspace.create_space("real-space", "new-owner-device", "/tmp", None, false).unwrap();
        workspace.create_chat(CHAT, "real-space", None, None).unwrap();
        workspace.record_session_activity(None, "owner", &Session {
            status: SessionStatus::Working, ..current.clone()
        }, None).unwrap();
        assert_eq!(workspace.doc().read_sessions().unwrap(), before);
        assert_eq!(workspace.doc().chat(CHAT).unwrap().unwrap().device_id, "new-owner-device");
        workspace.remove_session_ref(CHAT).unwrap();
        receive_for(&mut sessions, "rowless membership removed", |rows| rows.iter().all(|row| row.chat_id != CHAT)).await;
        workspace.record_session_activity(None, "owner", current, None).unwrap();
        assert!(workspace.doc().session_ref("owner", CHAT).unwrap().is_none());
        assert_eq!(workspace.doc().read_sessions().unwrap(), before);
        drop(bridge);
    }

    #[tokio::test]
    async fn ordinary_remote_room_status_survives_stale_workspace_and_fences_owners() {
        const REMOTE_CHAT: &str = "00000000-0000-4000-8000-000000000001";
        const LOCAL_CHAT: &str = "00000000-0000-4000-8000-000000000002";
        let setup = |device: &str| {
            let temp = tempfile::tempdir().unwrap();
            let store = std::sync::Arc::new(comet_sync::DocsStore::open(temp.path()).unwrap());
            let workspace = WorkspaceHost::open(store.clone(), crate::WorkspaceHostConfig {
                device_id: device.into(), device_name: "Test".into(), platform: "test".into(),
                project_scope: "project".into(), user_id: "owner".into(), edge: None,
            }).unwrap();
            workspace.create_space("remote-space", "remote", "/tmp", None, false).unwrap();
            workspace.create_chat(REMOTE_CHAT, "remote-space", None, None).unwrap();
            let host = DocHost::new(store, crate::DocHostConfig {
                device_id: device.into(), default_harness: HarnessId::Mock, edge: None,
            });
            host.set_workspace(workspace.clone());
            (temp, workspace, host)
        };
        let (_remote_temp, remote_workspace, remote) = setup("remote");
        let (_desktop_temp, workspace, desktop) = setup("desktop");
        workspace.create_space("local-space", "desktop", "/tmp/local", None, false).unwrap();
        workspace.create_chat(LOCAL_CHAT, "local-space", None, None).unwrap();
        let remote_room = remote.open(REMOTE_CHAT).unwrap();
        let desktop_room = desktop.open(REMOTE_CHAT).unwrap();
        let stale_at = DateTime::from_timestamp_millis(crate::now_ms() - comet_proto::view::SESSION_STALE_MS - 1_000).unwrap();
        let mut current = Session {
            chat_id: REMOTE_CHAT.into(), device_id: "remote".into(),
            status: SessionStatus::Working, started_at: None,
            model_retry: None,
            updated_at: stale_at,
        };
        // Existing bare-chat agents acquire the canonical record on their next
        // status/heartbeat, without a Start command or a different execution key.
        remote.record_agent_session(&current).unwrap();
        remote_room.doc().push_message(&SessionMessageEntry {
            id: "old-stream".into(), role: MessageRole::Assistant,
            parts: vec![MessagePart::Text { id: "text".into(), text: "old content".into() }],
            created_at: stale_at.timestamp_millis() + 100, device_id: "remote".into(), status: Some(MessageStatus::Streaming),
            continuation_of: None, peer_message: None,
        }).unwrap();
        let sync_room = || {
            desktop_room.doc().binding().import(&remote_room.doc().export_snapshot().unwrap()).unwrap();
        };
        sync_room();
        let mut local = Session {
            chat_id: LOCAL_CHAT.into(), device_id: "desktop".into(),
            status: SessionStatus::Working, started_at: None,
            model_retry: None,
            updated_at: Utc::now(),
        };
        let abandoned = Session { device_id: "desktop".into(), status: SessionStatus::Idle, ..current.clone() };
        let (local_tx, local_rx) = watch::channel(vec![local.clone(), abandoned]);
        let mut sessions = workspace.merged_sessions_watch(local_rx);
        let mut chats = workspace.watch_chats();
        let bridge = SessionActivity::start(desktop, workspace.clone());
        let rows = receive_for(&mut sessions, "initial stale remote owner", |rows| rows.iter().any(|row| {
            row.chat_id == REMOTE_CHAT && row.device_id == "remote"
        })).await;
        let old = rows.iter().find(|row| row.chat_id == REMOTE_CHAT).unwrap();
        assert_eq!(old.updated_at, stale_at);
        assert_eq!(comet_proto::view::effective_indicator(Some(old),
            Utc::now()), comet_proto::view::Indicator::Unreachable,
            "a static streaming snapshot is not a current heartbeat");

        for (status, sidebar) in [
            (SessionStatus::Working, ChatIndicator::Working),
            (SessionStatus::Working, ChatIndicator::Working),
            (SessionStatus::AwaitingInput, ChatIndicator::AwaitingInput),
            (SessionStatus::Working, ChatIndicator::Working),
            (SessionStatus::Idle, ChatIndicator::Completed),
            (SessionStatus::Working, ChatIndicator::Working),
            (SessionStatus::Errored, ChatIndicator::Errored),
        ] {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            current.status = status;
            current.updated_at = DateTime::from_timestamp_millis(crate::now_ms()).unwrap();
            remote.record_agent_session(&current).unwrap();
            sync_room();
            receive_for(&mut sessions, &format!("fresh owner {status:?} before stale import"),
                |rows| rows.iter().any(|row| row == &current)).await;
            let stale = Session { status: SessionStatus::Idle,
                updated_at: stale_at - chrono::Duration::milliseconds(1), ..current.clone() };
            // A real owner publishes its stale cached index, received as a
            // workspace import. A controller cannot forge that owner row locally.
            remote_workspace.doc().binding().import(&workspace.doc().export_snapshot().unwrap()).unwrap();
            // Force the same-room membership settlement that independently
            // created workspace refs can produce. No room update follows it.
            let mut settled = remote_workspace.doc().session_ref("owner", REMOTE_CHAT).unwrap().unwrap();
            settled.added_at = workspace.doc().session_ref("owner", REMOTE_CHAT).unwrap().unwrap().added_at
                + chrono::Duration::milliseconds(1);
            remote_workspace.doc().upsert_session_ref("owner", &settled).unwrap();
            remote_workspace.record_session(&stale);
            let before = workspace.doc().doc().oplog_vv();
            workspace.doc().binding().import(&remote_workspace.doc().doc().export(loro::ExportMode::updates(&before)).unwrap()).unwrap();
            assert_eq!(workspace.doc().read_sessions().unwrap().iter()
                .find(|row| row.chat_id == REMOTE_CHAT), Some(&stale),
                "the real owner's stale index must actually be imported");
            // A queued room delivery may restore the raw index before its watch
            // materializes. Only the merged consumer's current owner is promised.
            // Advance local live state to require a recomputation after the import,
            // rather than accepting the consumer's already-observed room value.
            local.updated_at += chrono::Duration::milliseconds(1);
            local_tx.send_replace(vec![local.clone()]);
            let rows = receive_for(&mut sessions, &format!("owner {status:?} after stale import"), |rows| {
                rows.iter().any(|row| row == &current) && rows.iter().any(|row| row == &local)
            }).await;
            assert_eq!(rows.iter().find(|row| row.chat_id == LOCAL_CHAT), Some(&local),
                "local live state still wins");
            let chat = workspace.doc().chat(REMOTE_CHAT).unwrap().unwrap();
            assert_eq!(comet_proto::view::display_status(&chat,
                rows.iter().find(|row| row.chat_id == REMOTE_CHAT), Utc::now()), sidebar);
        }
        let before = workspace.doc().read_sessions().unwrap();
        let foreign = Session { device_id: "foreign".into(), status: SessionStatus::Working, ..current.clone() };
        workspace.record_session_activity(None, "owner", &foreign, Some(crate::now_ms())).unwrap();
        workspace.record_session_activity(None, "foreign-principal", &current, Some(crate::now_ms())).unwrap();
        assert_eq!(workspace.doc().read_sessions().unwrap(), before);
        assert_eq!(sessions.borrow().iter().find(|row| row.chat_id == REMOTE_CHAT), Some(&current));
        local_tx.send_replace(vec![local]);
        workspace.remove_session_ref(REMOTE_CHAT).unwrap();
        receive_for(&mut chats, "unpinned chat removal", |rows| rows.iter().all(|row| row.id != REMOTE_CHAT)).await;
        receive_for(&mut sessions, "unpinned session removal", |rows| rows.iter().all(|row| row.chat_id != REMOTE_CHAT)).await;
        workspace.record_session_activity(None, "owner", &current, Some(crate::now_ms())).unwrap();
        assert_eq!(workspace.doc().read_sessions().unwrap(), before);
        drop(bridge);
    }

    #[tokio::test]
    async fn remote_activity_drives_unselected_sidebar_and_reconnect_preserves_seen() {
        const CHAT: &str = "00000000-0000-4000-8000-000000000003";
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
                CHAT,
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
                        session_id: Some(CHAT.into()),
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
        mismatched.environment.as_mut().unwrap().scope.session_id = Some(CHAT.into());
        mismatched.environment.as_mut().unwrap().scope.deployment_id = Some(" ".into());
        assert!(projection(&mismatched, "project").is_none());
        let handle = host.open_projection(CHAT, Some(&room)).unwrap();
        let started_at = crate::now_ms();
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
                        chat_id: CHAT.into(),
                        owner_subject: "owner".into(),
                        owner_device_id: "comet-scaffold-sandbox-e1".into(),
                        source: AgentSessionSource::Scaffold,
                        environment: None,
                        harness: None,
                        model: None,
                        harness_session_id: None,
                        status: Some(status),
                        model_retry: None,
                        started_at: None,
                        updated_at: Some(at),
                        created_at: started_at,
                        unknown: Default::default(),
                    })),
                    unknown: Default::default(),
                })
                .unwrap();
        };
        publish(CHAT, SessionStatus::Working, started_at);
        let message_at = crate::now_ms();
        handle
            .doc()
            .push_message(&SessionMessageEntry {
                id: "answer".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "text".into(),
                    text: "Working".into(),
                }],
                created_at: message_at,
                device_id: "comet-scaffold-sandbox-e1".into(),
                status: Some(MessageStatus::Streaming),
                continuation_of: None,
                peer_message: None,
            })
            .unwrap();
        let stale_local = Session {
            chat_id: CHAT.into(),
            device_id: "local".into(),
            status: SessionStatus::Idle,
            model_retry: None,
            started_at: None,
            updated_at: DateTime::from_timestamp_millis(crate::now_ms()).unwrap(),
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
                    .is_some_and(|at| at.timestamp_millis() == message_at)
            })
        })
        .await;
        workspace.set_chat_archived(CHAT, true).unwrap();
        let archived = receive(&mut chats, |chats| chats[0].archived).await;
        assert!(
            projected_rooms(std::slice::from_ref(&reference), &archived, &rows, "project", "local", crate::now_ms()).is_empty(),
            "archived sessions must not keep room observers alive"
        );
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while std::sync::Arc::strong_count(&handle) > 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("archived room observer stayed alive");
        workspace.set_chat_archived(CHAT, false).unwrap();
        receive(&mut chats, |chats| !chats[0].archived).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while std::sync::Arc::strong_count(&handle) < 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unarchived room observer did not restart");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let now = Utc::now();
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), now),
            ChatIndicator::Working
        );
        let newer_local = comet_proto::Chat {
            id: "newer-local".into(),
            last_message_at: Some(now),
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

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        publish(CHAT, SessionStatus::AwaitingInput, crate::now_ms());
        let rows = receive(&mut sessions, |rows| {
            rows[0].status == SessionStatus::AwaitingInput
        })
        .await;
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), Utc::now()),
            ChatIndicator::AwaitingInput
        );
        // A completed assistant segment (including text commentary) is not a turn boundary.
        handle
            .doc()
            .set_message_status("answer", MessageStatus::Complete)
            .unwrap();
        publish("other-agent", SessionStatus::Idle, crate::now_ms());
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let working_at = crate::now_ms();
        publish(CHAT, SessionStatus::Working, working_at);
        let rows = receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == working_at
        })
        .await;
        assert_eq!(rows[0].status, SessionStatus::Working);

        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let completed_at = crate::now_ms();
        let completed_time = DateTime::from_timestamp_millis(completed_at).unwrap();
        publish(CHAT, SessionStatus::Idle, completed_at);
        let rows = receive(&mut sessions, |rows| rows[0].status == SessionStatus::Idle).await;
        let list = receive(&mut chats, |chats| {
            chats[0]
                .last_message_at
                .is_some_and(|at| at.timestamp_millis() == completed_at)
        })
        .await;
        assert_eq!(
            comet_proto::view::display_status(&list[0], rows.first(), Utc::now()),
            ChatIndicator::Completed
        );
        let mut order = vec![
            (ChatIndicator::Idle, &newer_local),
            (ChatIndicator::Completed, &list[0]),
        ];
        comet_proto::view::sort_active(&mut order);
        assert_eq!(
            order[0].1.id, CHAT,
            "remote completion moves above newer local activity"
        );
        let seen_at = DateTime::from_timestamp_millis(crate::now_ms()).unwrap();
        workspace.doc().set_chat_seen(CHAT, seen_at).unwrap();
        workspace.rename_chat(CHAT, "Keep my title").unwrap();
        drop(bridge);
        let bridge = SessionActivity::start(host.clone(), workspace.clone());
        // A replayed real prior Working phase must not rewind the completed owner.
        publish(CHAT, SessionStatus::Working, working_at);
        assert_eq!(handle.doc().collaboration_snapshot().unwrap().sessions.iter()
            .find(|record| record.session_id == CHAT).unwrap().status, Some(SessionStatus::Idle));
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let reconnected_at = crate::now_ms();
        publish(CHAT, SessionStatus::Idle, reconnected_at);
        receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == reconnected_at
        })
        .await;
        let list = receive(&mut chats, |chats| {
            chats[0].last_seen_at == Some(seen_at) && chats[0].title.as_deref() == Some("Keep my title")
        })
        .await;
        assert_eq!(list[0].last_message_at, Some(completed_time));
        assert_eq!(
            comet_proto::view::display_status(&list[0], sessions.borrow().first(), Utc::now()),
            ChatIndicator::Idle
        );
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let idle_heartbeat_at = crate::now_ms();
        publish(CHAT, SessionStatus::Idle, idle_heartbeat_at);
        receive(&mut sessions, |rows| {
            rows[0].updated_at.timestamp_millis() == idle_heartbeat_at
        })
        .await;
        assert_eq!(
            workspace
                .doc()
                .chat(CHAT)
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(completed_time),
            "a redundant idle publication is status, not message activity"
        );

        workspace.remove_session_ref(CHAT).unwrap();
        receive(&mut chats, Vec::is_empty).await;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let unpinned_at = crate::now_ms();
        publish(CHAT, SessionStatus::Working, unpinned_at);
        // Even a queued observer delivery cannot write after the synchronous unpin.
        workspace
            .record_session_activity(
                Some(&room),
                "owner",
                &Session {
                    status: SessionStatus::Working,
                    updated_at: DateTime::from_timestamp_millis(unpinned_at).unwrap(),
                    ..rows[0].clone()
                },
                Some(unpinned_at),
            )
            .unwrap();
        assert_eq!(
            workspace
                .doc()
                .chat(CHAT)
                .unwrap()
                .unwrap()
                .last_message_at,
            Some(completed_time)
        );
        drop(bridge);
    }
}
