//! Poll loop: owns message polling and acking; scaling decisions are delegated
//! to a [`Scaler`]. Mirrors `listener/listener.go` upstream.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::session::MessageSession;
use crate::types::ScaleSetMessage;
use crate::{Error, Result};

/// Message id of the synthetic first message carrying session statistics.
/// Real messages are numbered from 0, so this never collides.
pub const INITIAL_MESSAGE_ID: i64 = -1;

/// Handles scale set messages.
///
/// `scale` is called once per poll and must tolerate:
/// - `None`: the long poll timed out. Use it for housekeeping.
/// - The initial message (`message_id == INITIAL_MESSAGE_ID`) with session statistics.
/// - Redelivery: a message is acked only after `scale` returns `Ok`, so an
///   error stops the listener and the same message is delivered again later.
///
/// Every `JobAvailable` the scaler wants must be passed to
/// [`MessageSession::acquire_jobs`], or the job stays unassigned.
#[async_trait::async_trait]
pub trait Scaler: Send {
    async fn scale(&mut self, session: &MessageSession, message: Option<&ScaleSetMessage>) -> Result<()>;
}

pub struct Listener {
    session: Arc<MessageSession>,
    max_runners: Arc<AtomicU32>,
}

impl Listener {
    pub fn new(session: Arc<MessageSession>, max_runners: u32) -> Self {
        Self { session, max_runners: Arc::new(AtomicU32::new(max_runners)) }
    }

    /// Handle for changing the advertised capacity while running.
    pub fn max_runners(&self) -> Arc<AtomicU32> {
        self.max_runners.clone()
    }

    pub async fn run<S: Scaler>(&self, scaler: &mut S, shutdown: impl std::future::Future<Output = ()>) -> Result<()> {
        let initial = self.session.session().await;
        let stats = initial.statistics.ok_or_else(|| Error::Protocol("session has no statistics".into()))?;
        scaler
            .scale(
                &self.session,
                Some(&ScaleSetMessage {
                    message_id: INITIAL_MESSAGE_ID,
                    statistics: Some(stats),
                    ..Default::default()
                }),
            )
            .await?;

        tokio::pin!(shutdown);
        let mut last_message_id = 0i64;
        loop {
            let poll = self.session.get_message(last_message_id, self.max_runners.load(Ordering::Relaxed));
            let msg = tokio::select! {
                _ = &mut shutdown => return Ok(()),
                m = poll => m?,
            };
            scaler.scale(&self.session, msg.as_ref()).await?;
            if let Some(m) = msg {
                last_message_id = m.message_id;
                // Ack even if shutdown is requested: the work is already done.
                self.session.delete_message(m.message_id).await?;
            }
        }
    }
}
