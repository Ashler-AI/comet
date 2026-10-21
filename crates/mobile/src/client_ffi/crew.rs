//! Crew uses the same resource-bound OAuth implementation as the host.
use std::sync::Arc;
use zeron_sync::scaffold_auth::{self, ScaffoldAuth, ScaffoldConfig, ScaffoldSession};
use super::{CoreClient, CoreError, CoreResult, Credentials};

#[derive(uniffi::Record)]
pub struct CrewAccount {
    pub credentials: Credentials,
    pub profile_key: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub project_scope: String,
}

#[derive(uniffi::Object)]
pub struct CrewLogin {
    auth: Arc<ScaffoldAuth>,
    control_origin: String,
}

fn error(e: scaffold_auth::AuthError) -> CoreError {
    zeron_client::auth::scaffold_error(e).into()
}

impl CrewLogin {
    fn account(&self, session: ScaffoldSession) -> CoreResult<CrewAccount> {
        let profile_key = scaffold_auth::profile_key(&session);
        let session_json = serde_json::to_string(&session)
            .map_err(|e| CoreError::Internal { message: e.to_string() })?;
        Ok(CrewAccount {
            credentials: Credentials::Scaffold {
                user_id: session.identity.user_id.clone(),
                org_id: session.identity.project_scope.clone(),
                control_origin: self.control_origin.clone(),
                session_json,
            },
            profile_key,
            name: session.identity.name,
            email: Some(session.identity.email),
            project_scope: session.identity.project_scope,
        })
    }
}

#[uniffi::export]
impl CrewLogin {
    #[uniffi::constructor]
    pub fn new(control_origin: String, edge_origin: String, project_scope: String, deployment_id: String) -> CoreResult<Arc<Self>> {
        let mut config = ScaffoldConfig::new(control_origin.clone(), edge_origin, project_scope);
        config.deployment_id = Some(deployment_id);
        let auth = ScaffoldAuth::new(config).map_err(error)?;
        Ok(Arc::new(Self { auth: Arc::new(auth), control_origin }))
    }

    pub async fn begin(&self, redirect_uri: String) -> CoreResult<String> {
        let auth = self.auth.clone();
        super::on_runtime(async move { auth.begin_sign_in(&redirect_uri).await.map_err(zeron_client::auth::scaffold_error) }).await
    }

    pub async fn complete(&self, callback: String) -> CoreResult<CrewAccount> {
        let auth = self.auth.clone();
        let session = super::on_runtime(async move { auth.complete_sign_in(&callback).await.map_err(zeron_client::auth::scaffold_error) }).await?;
        self.account(session)
    }

    pub async fn restore(&self, session_json: String) -> CoreResult<CrewAccount> {
        let session: ScaffoldSession = serde_json::from_str(&session_json)
            .map_err(|_| CoreError::Auth { message: "Invalid saved Crew session".into() })?;
        let auth = self.auth.clone();
        let session = super::on_runtime(async move { auth.verify(&session).await.map_err(zeron_client::auth::scaffold_error) }).await?;
        self.account(session)
    }

    pub fn cancel(&self) { self.auth.cancel_sign_in(); }
}

#[uniffi::export]
pub fn crew_is_session_link(url: String) -> bool {
    zeron_client::crew::parse_session_link(&url).is_some()
}

#[uniffi::export]
impl CoreClient {
    pub fn crew_cached_link(&self, url: String, deployment: String) -> CoreResult<Option<String>> {
        self.client.crew_cached_link(&url, &deployment).map_err(Into::into)
    }

    pub async fn crew_resolve_link(&self, device_id: String, url: String, deployment: String) -> CoreResult<String> {
        let client = self.client.clone();
        super::on_runtime(async move { client.crew_resolve_link(&device_id, &url, &deployment).await }).await
    }

    pub async fn crew_control(&self, device_id: String, command_id: String, control_json: String, deployment: String) -> CoreResult<String> {
        let control = serde_json::from_str(&control_json)
            .map_err(|e| CoreError::InvalidArgument { message: format!("Invalid Scaffold operation: {e}") })?;
        let client = self.client.clone();
        let value = super::on_runtime(async move { client.crew_control(&device_id, &command_id, control, &deployment).await }).await?;
        Ok(value.to_string())
    }

    pub async fn crew_reconcile(&self, device_id: String, command_id: String) -> CoreResult<String> {
        let client = self.client.clone();
        let value = super::on_runtime(async move { client.crew_reconcile(&device_id, &command_id).await }).await?;
        Ok(value.to_string())
    }

    pub async fn crew_refresh(&self, device_id: String, deployment: String) -> CoreResult<String> {
        let client = self.client.clone();
        let value = super::on_runtime(async move { client.crew_refresh(&device_id, &deployment).await }).await?;
        Ok(value.to_string())
    }

    pub async fn crew_import_legacy_backup(&self, source_path: String) -> CoreResult<String> {
        let client = self.client.clone();
        super::on_runtime(async move { client.crew_import_legacy_backup(source_path.into()).await }).await
    }
}
