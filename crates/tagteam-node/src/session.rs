//! Reconnecting bidirectional node session and bounded command-layer message surface.
//!
//! One task owns each tonic stream. Caller messages and surfaced hub events use bounded
//! channels; a terminal server revocation or `REMOVED` welcome ends the task permanently.

use std::{sync::Arc, time::Duration};

use tokio::{
    sync::{mpsc, watch},
    time::{Instant, interval_at},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Code, Status};

use crate::{NODE_VERSION, NodeIdentity, transport};

const MESSAGE_CAPACITY: usize = 16;
const EVENT_CAPACITY: usize = 16;

/// Supplies deterministic or platform-probed heartbeat data to the session.
pub trait HeartbeatSource: Send + Sync + 'static {
    fn heartbeat(&self) -> tagteam_proto::v1::Heartbeat;
}

/// Session inputs copied into each reconnect's initial Hello message.
pub struct NodeSession {
    identity: NodeIdentity,
    version: String,
    workspaces: Vec<String>,
    pending: Vec<tagteam_proto::v1::CommandAck>,
    heartbeat_source: Arc<dyn HeartbeatSource>,
    reconnect: ReconnectPolicy,
}

impl NodeSession {
    pub fn new(identity: NodeIdentity, heartbeat_source: impl HeartbeatSource) -> Self {
        Self {
            identity,
            version: NODE_VERSION.to_owned(),
            workspaces: Vec::new(),
            pending: Vec::new(),
            heartbeat_source: Arc::new(heartbeat_source),
            reconnect: ReconnectPolicy::default(),
        }
    }

    pub fn hello_state(
        mut self,
        version: impl Into<String>,
        workspaces: Vec<String>,
        pending: Vec<tagteam_proto::v1::CommandAck>,
    ) -> Self {
        self.version = version.into();
        self.workspaces = workspaces;
        self.pending = pending;
        self
    }

    pub fn reconnect_policy(mut self, policy: ReconnectPolicy) -> Self {
        self.reconnect = policy;
        self
    }

    /// Start the session loop. Dropping event consumption backpressures the bounded queue.
    pub fn start(self) -> SessionHandle {
        let (messages, message_receiver) = mpsc::channel(MESSAGE_CAPACITY);
        let (events, event_receiver) = mpsc::channel(EVENT_CAPACITY);
        let (shutdown, shutdown_receiver) = watch::channel(false);
        tokio::spawn(self.run(message_receiver, events, shutdown_receiver));
        SessionHandle {
            messages,
            events: event_receiver,
            shutdown,
        }
    }

    async fn run(
        self,
        mut messages: mpsc::Receiver<tagteam_proto::v1::NodeMessage>,
        events: mpsc::Sender<SessionEvent>,
        mut shutdown: watch::Receiver<bool>,
    ) {
        let mut attempt = 0_u32;
        loop {
            if *shutdown.borrow() {
                return;
            }
            match self
                .connect_and_run(&mut messages, &events, &mut shutdown)
                .await
            {
                Ok(()) => return,
                Err(error) if error.is_terminal() => {
                    let _ = events.send(SessionEvent::Terminated(error)).await;
                    return;
                }
                Err(_) => {
                    let delay = self.reconnect.delay(attempt);
                    attempt = attempt.saturating_add(1);
                    if events
                        .send(SessionEvent::Reconnecting { attempt, delay })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {},
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() { return; }
                        }
                    }
                }
            }
        }
    }

    async fn connect_and_run(
        &self,
        messages: &mut mpsc::Receiver<tagteam_proto::v1::NodeMessage>,
        events: &mpsc::Sender<SessionEvent>,
        shutdown: &mut watch::Receiver<bool>,
    ) -> Result<(), SessionError> {
        let channel = transport::session_channel(&self.identity)
            .await
            .map_err(|_| SessionError::Channel)?;
        let (stream_sender, stream_receiver) = mpsc::channel(MESSAGE_CAPACITY);
        stream_sender
            .send(tagteam_proto::v1::NodeMessage {
                msg: Some(tagteam_proto::v1::node_message::Msg::Hello(
                    tagteam_proto::v1::Hello {
                        node_id: self.identity.node_id.to_string(),
                        version: self.version.clone(),
                        workspaces: self.workspaces.clone(),
                        pending: self.pending.clone(),
                    },
                )),
            })
            .await
            .map_err(|_| SessionError::Channel)?;
        let mut client = tagteam_proto::v1::hub_client::HubClient::new(channel);
        let response = client
            .session(ReceiverStream::new(stream_receiver))
            .await
            .map_err(status_error)?;
        let mut incoming = response.into_inner();
        let welcome = incoming
            .message()
            .await
            .map_err(status_error)?
            .ok_or(SessionError::ClosedBeforeWelcome)?;
        let Some(tagteam_proto::v1::hub_message::Msg::Welcome(welcome)) = welcome.msg else {
            return Err(SessionError::InvalidWelcome);
        };
        if welcome.session_id.is_empty() || welcome.heartbeat_ms == 0 {
            return Err(SessionError::InvalidWelcome);
        }
        if welcome.node_state == tagteam_proto::v1::NodeState::Removed as i32 {
            return Err(SessionError::Removed);
        }
        let heartbeat_interval = Duration::from_millis(u64::from(welcome.heartbeat_ms));
        events
            .send(SessionEvent::Welcome(welcome))
            .await
            .map_err(|_| SessionError::Closed)?;
        stream_sender
            .send(tagteam_proto::v1::NodeMessage {
                msg: Some(tagteam_proto::v1::node_message::Msg::Heartbeat(
                    self.heartbeat_source.heartbeat(),
                )),
            })
            .await
            .map_err(|_| SessionError::Closed)?;
        let mut heartbeats = interval_at(Instant::now() + heartbeat_interval, heartbeat_interval);

        loop {
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return Ok(()); }
                }
                message = messages.recv() => match message {
                    Some(message) => stream_sender.send(message).await.map_err(|_| SessionError::Closed)?,
                    None => return Ok(()),
                },
                _ = heartbeats.tick() => {
                    stream_sender.send(tagteam_proto::v1::NodeMessage {
                        msg: Some(tagteam_proto::v1::node_message::Msg::Heartbeat(
                            self.heartbeat_source.heartbeat(),
                        )),
                    }).await.map_err(|_| SessionError::Closed)?;
                }
                message = incoming.message() => match message {
                    Ok(Some(message)) => {
                        let event = match message.msg {
                            Some(tagteam_proto::v1::hub_message::Msg::Command(value)) => Some(SessionEvent::Command(value)),
                            Some(tagteam_proto::v1::hub_message::Msg::Cancel(value)) => Some(SessionEvent::Cancel(value)),
                            Some(tagteam_proto::v1::hub_message::Msg::Drain(value)) => Some(SessionEvent::Drain(value)),
                            Some(tagteam_proto::v1::hub_message::Msg::Welcome(_)) | None => return Err(SessionError::InvalidWelcome),
                        };
                        if let Some(event) = event {
                            events.send(event).await.map_err(|_| SessionError::Closed)?;
                        }
                    }
                    Ok(None) => return Err(SessionError::Closed),
                    Err(status) => return Err(status_error(status)),
                }
            }
        }
    }
}

/// Bounded exponential retry schedule used after transport or stream closure.
#[derive(Clone, Copy, Debug)]
pub struct ReconnectPolicy {
    pub initial: Duration,
    pub maximum: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            maximum: Duration::from_secs(30),
        }
    }
}

impl ReconnectPolicy {
    pub fn delay(self, attempt: u32) -> Duration {
        self.initial
            .checked_mul(1_u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX))
            .unwrap_or(self.maximum)
            .min(self.maximum)
    }
}

/// Messages sent by the hub to the node command layer.
pub enum SessionEvent {
    Welcome(tagteam_proto::v1::Welcome),
    Command(tagteam_proto::v1::Command),
    Cancel(tagteam_proto::v1::Cancel),
    Drain(tagteam_proto::v1::Drain),
    Reconnecting { attempt: u32, delay: Duration },
    Terminated(SessionError),
}

/// Handle used to send node messages and receive bounded hub events.
pub struct SessionHandle {
    messages: mpsc::Sender<tagteam_proto::v1::NodeMessage>,
    events: mpsc::Receiver<SessionEvent>,
    shutdown: watch::Sender<bool>,
}

impl SessionHandle {
    pub async fn send_output(
        &self,
        output: tagteam_proto::v1::OutputChunk,
    ) -> Result<(), SessionError> {
        self.send(tagteam_proto::v1::node_message::Msg::Output(output))
            .await
    }

    pub async fn send_result(
        &self,
        result: tagteam_proto::v1::CommandResult,
    ) -> Result<(), SessionError> {
        self.send(tagteam_proto::v1::node_message::Msg::Result(result))
            .await
    }

    /// Send the command result that acknowledges completion on the active stream.
    pub async fn send_ack(&self, ack: tagteam_proto::v1::CommandAck) -> Result<(), SessionError> {
        let result = ack.result.ok_or(SessionError::InvalidAcknowledgement)?;
        if result.command_id != ack.command_id {
            return Err(SessionError::InvalidAcknowledgement);
        }
        self.send_result(result).await
    }

    async fn send(
        &self,
        message: tagteam_proto::v1::node_message::Msg,
    ) -> Result<(), SessionError> {
        self.messages
            .send(tagteam_proto::v1::NodeMessage { msg: Some(message) })
            .await
            .map_err(|_| SessionError::Closed)
    }

    pub async fn next_event(&mut self) -> Option<SessionEvent> {
        self.events.recv().await
    }

    pub fn shutdown(&self) {
        self.shutdown.send_replace(true);
    }
}

/// Safe session failures; status payloads and transport diagnostics are not exposed.
#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionError {
    #[error("hub connection failed")]
    Channel,
    #[error("hub session closed")]
    Closed,
    #[error("hub closed before Welcome")]
    ClosedBeforeWelcome,
    #[error("hub sent an invalid Welcome")]
    InvalidWelcome,
    #[error("node was removed by the hub")]
    Removed,
    #[error("hub revoked node authorization")]
    Unauthenticated,
    #[error("command acknowledgement is incomplete")]
    InvalidAcknowledgement,
    #[error("hub session failed ({0:?})")]
    Rpc(Code),
}

impl SessionError {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Removed | Self::Unauthenticated)
    }
}

fn status_error(status: Status) -> SessionError {
    match status.code() {
        Code::Unauthenticated => SessionError::Unauthenticated,
        code => SessionError::Rpc(code),
    }
}

#[cfg(test)]
mod tests {
    use super::ReconnectPolicy;
    use std::time::Duration;

    #[test]
    fn reconnect_backoff_grows_and_stays_bounded() {
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(100),
            maximum: Duration::from_millis(450),
        };
        assert_eq!(policy.delay(0), Duration::from_millis(100));
        assert_eq!(policy.delay(1), Duration::from_millis(200));
        assert_eq!(policy.delay(2), Duration::from_millis(400));
        assert_eq!(policy.delay(3), Duration::from_millis(450));
        assert_eq!(policy.delay(u32::MAX), Duration::from_millis(450));
    }
}
