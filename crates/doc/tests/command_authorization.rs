use std::collections::BTreeMap;

use comet_doc::{
    SessionCommandEntry, SessionCommandPayload, SessionCommandStatus, SessionControlAction,
    authorize_control_command,
};
use comet_proto::{
    AgentSessionSource, CAPABILITY_SESSION_ANNOTATE, CAPABILITY_SESSION_CHAT,
    CAPABILITY_SESSION_CONTROL, CapabilityGrant, CollaborationPrincipal, CollaborationScope,
};

fn principal(capabilities: &[&str]) -> CollaborationPrincipal {
    CollaborationPrincipal {
        subject: "iap:chris@example.com".into(),
        email: Some("chris@example.com".into()),
        project_id: "project-a".into(),
        deployment_id: Some("deployment-a".into()),
        session_id: Some("thread-a".into()),
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        unknown: BTreeMap::new(),
    }
}

fn grant(capabilities: &[&str]) -> CapabilityGrant {
    CapabilityGrant {
        id: "grant-a".into(),
        principal_subject: "iap:chris@example.com".into(),
        scope: CollaborationScope {
            project_id: "project-a".into(),
            deployment_id: Some("deployment-a".into()),
            session_id: Some("thread-a".into()),
            unknown: BTreeMap::new(),
        },
        capabilities: capabilities.iter().map(|value| (*value).into()).collect(),
        sandbox_id: None,
        device_id: Some("device-chris".into()),
        granted_by: "iap:admin@example.com".into(),
        granted_at: 1,
        expires_at: Some(1_000),
        revoked_at: None,
        unknown: BTreeMap::new(),
    }
}

fn pause_command() -> SessionCommandEntry {
    SessionCommandEntry {
        id: "command-a".into(),
        payload: SessionCommandPayload::Control {
            session_id: "session-kyle".into(),
            owner_device_id: "device-kyle".into(),
            actor_device_id: "device-chris".into(),
            actor_subject: "iap:chris@example.com".into(),
            grant_id: "grant-a".into(),
            source: AgentSessionSource::Local,
            action: Box::new(SessionControlAction::Pause {}),
        },
        issued_by: "device-chris".into(),
        issued_at: 10,
        based_on: None,
        expires_at: None,
        status: SessionCommandStatus::Pending,
        resolution: None,
    }
}

#[test]
fn unauthorized_control_command_is_rejected() {
    let command = pause_command();
    let chat_only = principal(&["session.chat"]);
    assert!(!authorize_control_command(
        &command,
        &chat_only,
        &[grant(&["session.chat"])],
        10
    ));
    assert!(!authorize_control_command(
        &command,
        &principal(&[CAPABILITY_SESSION_CONTROL]),
        &[grant(&[CAPABILITY_SESSION_CONTROL])],
        1_000,
    ));
}

#[test]
fn verified_scoped_grant_authorizes_exact_capability() {
    let command = pause_command();
    assert!(authorize_control_command(
        &command,
        &principal(&[CAPABILITY_SESSION_CONTROL]),
        &[grant(&[CAPABILITY_SESSION_CONTROL])],
        10,
    ));
    let mut wrong_actor = principal(&[CAPABILITY_SESSION_CONTROL]);
    wrong_actor.subject = "iap:kyle@example.com".into();
    assert!(!authorize_control_command(
        &command,
        &wrong_actor,
        &[grant(&[CAPABILITY_SESSION_CONTROL])],
        10,
    ));
}

#[test]
fn annotation_edits_require_annotate_capability() {
    let action = SessionControlAction::AnnotationEdit {
        annotation_id: "annotation-1".into(),
        body: Some("Updated review".into()),
        anchor: None,
    };
    assert_eq!(action.required_capability(), CAPABILITY_SESSION_ANNOTATE);
}

#[test]
fn scoped_input_answers_require_chat_capability() {
    let action = SessionControlAction::RespondInput {
        request_id: "input-a".into(),
        answers: vec![],
    };
    assert_eq!(action.required_capability(), CAPABILITY_SESSION_CHAT);
    let mut command = pause_command();
    let SessionCommandPayload::Control {
        action: command_action,
        ..
    } = &mut command.payload
    else {
        unreachable!();
    };
    **command_action = action;
    assert!(
        !authorize_control_command(
            &command,
            &principal(&[CAPABILITY_SESSION_CONTROL]),
            &[grant(&[CAPABILITY_SESSION_CONTROL])],
            10,
        ),
        "session.control alone cannot answer chat input"
    );
    assert!(authorize_control_command(
        &command,
        &principal(&[CAPABILITY_SESSION_CHAT]),
        &[grant(&[CAPABILITY_SESSION_CHAT])],
        10,
    ));
}
