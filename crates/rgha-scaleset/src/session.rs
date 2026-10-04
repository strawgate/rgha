//! Message session: the long-poll queue that tells a scale set about jobs.

use tokio::sync::{Mutex, RwLock};

use crate::client::{
    API_VERSION, Client, SCALE_SET_ENDPOINT, expect_json, expect_status, http_error, read_body, redact,
};
use crate::types::*;
use crate::{Error, Result};

pub const HEADER_MAX_CAPACITY: &str = "X-ScaleSetMaxCapacity";

pub struct MessageSession {
    client: Client,
    scale_set_id: i64,
    session: RwLock<Session>,
    refresh: Mutex<()>,
}

impl MessageSession {
    pub(crate) async fn create(client: Client, scale_set_id: i64, owner: &str) -> Result<Self> {
        let body = serde_json::to_value(Session { owner_name: owner.to_string(), ..Default::default() })?;
        let req = client
            .actions_request(
                reqwest::Method::POST,
                &format!("{SCALE_SET_ENDPOINT}/{scale_set_id}/sessions"),
                &[],
                Some(body),
            )
            .await?
            .build()
            .map_err(Error::build)?;
        let session: Session = expect_json(client.send(req).await?, 200).await?;
        if session.session_id.is_none() || session.message_queue_url.is_empty() {
            return Err(Error::Protocol("session response missing id or queue url".into()));
        }
        Ok(Self { client, scale_set_id, session: RwLock::new(session), refresh: Mutex::new(()) })
    }

    pub async fn session(&self) -> Session {
        self.session.read().await.clone()
    }

    pub fn scale_set_id(&self) -> i64 {
        self.scale_set_id
    }

    /// Refreshes the queue token unless another task already did.
    async fn refresh(&self, expired: &Session) -> Result<()> {
        let _g = self.refresh.lock().await;
        let current = self.session().await;
        if current.session_id != expired.session_id
            || current.message_queue_access_token != expired.message_queue_access_token
        {
            return Ok(());
        }
        let id = current.session_id.expect("validated at create");
        let req = self
            .client
            .actions_request(
                reqwest::Method::PATCH,
                &format!("{SCALE_SET_ENDPOINT}/{}/sessions/{id}", self.scale_set_id),
                &[],
                None,
            )
            .await?
            .build()
            .map_err(Error::build)?;
        let refreshed: Session = expect_json(self.client.send(req).await?, 200).await?;
        *self.session.write().await = refreshed;
        Ok(())
    }

    /// Long-polls (~50s) for the next message. `Ok(None)` means the poll timed
    /// out with nothing new. Messages are redelivered until deleted.
    pub async fn get_message(&self, last_message_id: i64, max_capacity: u32) -> Result<Option<ScaleSetMessage>> {
        let s = self.session().await;
        match self.get_message_once(&s, last_message_id, max_capacity).await {
            Err(Error::QueueTokenExpired) => {
                self.refresh(&s).await?;
                self.get_message_once(&self.session().await, last_message_id, max_capacity).await
            }
            other => other,
        }
    }

    async fn get_message_once(
        &self,
        s: &Session,
        last_message_id: i64,
        max_capacity: u32,
    ) -> Result<Option<ScaleSetMessage>> {
        let mut url =
            url::Url::parse(&s.message_queue_url).map_err(|e| Error::Protocol(format!("bad queue url: {e}")))?;
        if last_message_id > 0 {
            url.query_pairs_mut().append_pair("lastMessageId", &last_message_id.to_string());
        }
        let req = self
            .client
            .inner
            .http
            .get(url)
            .header("Accept", format!("application/json; api-version={API_VERSION}"))
            .header("Authorization", format!("Bearer {}", s.message_queue_access_token))
            .header("User-Agent", &self.client.inner.user_agent)
            .header(HEADER_MAX_CAPACITY, max_capacity.to_string())
            .build()
            .map_err(Error::build)?;
        let resp = self.client.send(req).await?;
        let url = redact(resp.url());
        let (status, activity, body) = read_body(resp).await;
        match status.as_u16() {
            202 => Ok(None),
            200 => {
                let raw: RawMessage =
                    serde_json::from_slice(&body).map_err(|e| Error::Protocol(format!("decoding message: {e}")))?;
                ScaleSetMessage::decode(raw).map(Some).map_err(Error::Protocol)
            }
            401 => Err(Error::QueueTokenExpired),
            _ => Err(http_error(status, activity, &body, &url)),
        }
    }

    /// Acknowledges a processed message.
    pub async fn delete_message(&self, message_id: i64) -> Result<()> {
        let s = self.session().await;
        match self.delete_message_once(&s, message_id).await {
            Err(Error::QueueTokenExpired) => {
                self.refresh(&s).await?;
                self.delete_message_once(&self.session().await, message_id).await
            }
            other => other,
        }
    }

    async fn delete_message_once(&self, s: &Session, message_id: i64) -> Result<()> {
        let mut url =
            url::Url::parse(&s.message_queue_url).map_err(|e| Error::Protocol(format!("bad queue url: {e}")))?;
        let path = format!("{}/{message_id}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let req = self
            .client
            .inner
            .http
            .delete(url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", s.message_queue_access_token))
            .header("User-Agent", &self.client.inner.user_agent)
            .build()
            .map_err(Error::build)?;
        let resp = self.client.send(req).await?;
        if resp.status().as_u16() == 401 {
            return Err(Error::QueueTokenExpired);
        }
        expect_status(resp, 204).await
    }

    /// Claims jobs for this scale set. Only acquired jobs get assigned to its
    /// runners; acquiring an already-acquired job is a no-op.
    pub async fn acquire_jobs(&self, request_ids: &[i64]) -> Result<Vec<i64>> {
        if request_ids.is_empty() {
            return Ok(vec![]);
        }
        let s = self.session().await;
        match self.acquire_once(&s, request_ids).await {
            Err(Error::QueueTokenExpired) => {
                self.refresh(&s).await?;
                self.acquire_once(&self.session().await, request_ids).await
            }
            other => other,
        }
    }

    async fn acquire_once(&self, s: &Session, request_ids: &[i64]) -> Result<Vec<i64>> {
        let req = self
            .client
            .actions_request(
                reqwest::Method::POST,
                &format!("{SCALE_SET_ENDPOINT}/{}/acquirejobs", self.scale_set_id),
                &[],
                Some(serde_json::to_value(request_ids)?),
            )
            .await?
            .build()
            .map_err(Error::build)?;
        // acquirejobs authenticates with the queue token, not the admin token.
        let mut req = req;
        req.headers_mut().insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", s.message_queue_access_token)
                .parse()
                .map_err(|_| Error::Protocol("queue token is not a valid header".into()))?,
        );
        let resp = self.client.send(req).await?;
        if resp.status().as_u16() == 401 {
            return Err(Error::QueueTokenExpired);
        }
        let list: ListResponse<i64> = expect_json(resp, 200).await?;
        Ok(list.value)
    }

    /// Deletes the session so another listener can take over.
    pub async fn close(&self) -> Result<()> {
        let s = self.session().await;
        let Some(id) = s.session_id else { return Ok(()) };
        let req = self
            .client
            .actions_request(
                reqwest::Method::DELETE,
                &format!("{SCALE_SET_ENDPOINT}/{}/sessions/{id}", self.scale_set_id),
                &[],
                None,
            )
            .await?
            .build()
            .map_err(Error::build)?;
        expect_status(self.client.send(req).await?, 204).await
    }
}
