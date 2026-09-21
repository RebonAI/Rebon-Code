//! The RC server, as the [`crate::ports`] traits see it: the bridge's HTTP
//! client and its session-stream client.

use std::sync::Arc;

use async_trait::async_trait;
use rebon_bridge::api_client::{BridgeApiClient, BridgeApiResult, PollOptions};
use rebon_bridge::config::{
    BridgeConfig, HeartbeatOutcome, PermissionResponseEvent, RegisteredEnvironment, WorkItem,
    WorkResponse,
};
use rebon_bridge::http_client::HttpBridgeApiClient;
use rebon_bridge::projects::ProjectInfo;
use rebon_bridge::session_stream::SessionFrame;
use rebon_bridge::stream_client::{
    CloseReason, SessionStream, SessionStreamError, SessionStreamOptions, SessionStreamRx,
    SessionStreamTx,
};

use crate::ports::{EnvironmentApi, FrameReceiver, FrameSender, StreamConnector};

/// The HTTP client as an [`EnvironmentApi`].
///
/// A newtype rather than an impl on the bridge's type, which lives in a
/// crate that must not know this one.
#[derive(Debug)]
pub struct HttpEnvironmentApi(pub HttpBridgeApiClient);

#[async_trait]
impl BridgeApiClient for HttpEnvironmentApi {
    async fn register_bridge_environment(
        &self,
        config: &BridgeConfig,
    ) -> BridgeApiResult<RegisteredEnvironment> {
        self.0.register_bridge_environment(config).await
    }

    async fn poll_for_work(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkResponse>> {
        self.0
            .poll_for_work(environment_id, environment_secret, options)
            .await
    }

    async fn poll_for_work_item(
        &self,
        environment_id: &str,
        environment_secret: &str,
        options: PollOptions,
    ) -> BridgeApiResult<Option<WorkItem>> {
        self.0
            .poll_for_work_item(environment_id, environment_secret, options)
            .await
    }

    async fn acknowledge_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<()> {
        self.0
            .acknowledge_work(environment_id, work_id, session_token)
            .await
    }

    async fn stop_work(
        &self,
        environment_id: &str,
        work_id: &str,
        force: bool,
    ) -> BridgeApiResult<()> {
        self.0.stop_work(environment_id, work_id, force).await
    }

    async fn deregister_environment(&self, environment_id: &str) -> BridgeApiResult<()> {
        self.0.deregister_environment(environment_id).await
    }

    async fn send_permission_response_event(
        &self,
        session_id: &str,
        event: &PermissionResponseEvent,
        session_token: &str,
    ) -> BridgeApiResult<()> {
        self.0
            .send_permission_response_event(session_id, event, session_token)
            .await
    }

    async fn archive_session(&self, session_id: &str) -> BridgeApiResult<()> {
        self.0.archive_session(session_id).await
    }

    async fn reconnect_session(
        &self,
        environment_id: &str,
        session_id: &str,
    ) -> BridgeApiResult<()> {
        self.0.reconnect_session(environment_id, session_id).await
    }

    async fn heartbeat_work(
        &self,
        environment_id: &str,
        work_id: &str,
        session_token: &str,
    ) -> BridgeApiResult<HeartbeatOutcome> {
        self.0
            .heartbeat_work(environment_id, work_id, session_token)
            .await
    }
}

#[async_trait]
impl EnvironmentApi for HttpEnvironmentApi {
    async fn update_projects(
        &self,
        environment_id: &str,
        projects: &[ProjectInfo],
    ) -> BridgeApiResult<()> {
        self.0
            .update_projects(environment_id, projects)
            .await
            .map(|_| ())
    }

    fn set_environment_secret(&self, secret: &str) {
        self.0.set_environment_secret(secret);
    }
}

/// Opens real session streams.
#[derive(Debug, Default)]
pub struct WsConnector;

#[async_trait]
impl StreamConnector for WsConnector {
    async fn connect(
        &self,
        ingress_url: &str,
        token: &str,
    ) -> Result<(Arc<dyn FrameSender>, Box<dyn FrameReceiver>), SessionStreamError> {
        let stream = SessionStream::connect(&SessionStreamOptions::new(ingress_url, token)).await?;
        let (tx, rx) = stream.split();
        Ok((Arc::new(WsSender(tx)), Box::new(WsReceiver(rx))))
    }
}

struct WsSender(SessionStreamTx);

#[async_trait]
impl FrameSender for WsSender {
    async fn send(&self, frame: &SessionFrame) -> Result<(), SessionStreamError> {
        self.0.send(frame).await
    }

    async fn close(&self) {
        if let Err(error) = self.0.close().await {
            tracing::debug!(%error, "rebon rc: the session stream did not close cleanly");
        }
    }
}

struct WsReceiver(SessionStreamRx);

#[async_trait]
impl FrameReceiver for WsReceiver {
    async fn recv(&mut self) -> Option<Result<SessionFrame, SessionStreamError>> {
        self.0.recv().await
    }

    fn close_reason(&self) -> Option<CloseReason> {
        self.0.close_reason().cloned()
    }
}
