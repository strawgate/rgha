//! Wire types for the Runner Scale Set API. Field names follow the JSON the
//! Actions service emits (mirrors `types.go` in actions/scaleset).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageType {
    JobAvailable,
    JobAssigned,
    JobStarted,
    JobCompleted,
    #[serde(other)]
    Unknown,
}

/// Fields common to every job message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobMessageBase {
    pub runner_request_id: i64,
    pub repository_name: String,
    pub owner_name: String,
    pub job_id: String,
    /// e.g. `owner/repo/.github/workflows/ci.yml@refs/heads/main`.
    pub job_workflow_ref: String,
    pub job_display_name: String,
    pub workflow_run_id: i64,
    pub event_name: String,
    pub request_labels: Vec<String>,
    pub queue_time: Option<DateTime<Utc>>,
    pub scale_set_assign_time: Option<DateTime<Utc>>,
    pub runner_assign_time: Option<DateTime<Utc>>,
    pub finish_time: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobAvailable {
    pub acquire_job_url: String,
    #[serde(flatten)]
    pub base: JobMessageBase,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobAssigned {
    #[serde(flatten)]
    pub base: JobMessageBase,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobStarted {
    pub runner_id: i64,
    pub runner_name: String,
    #[serde(flatten)]
    pub base: JobMessageBase,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct JobCompleted {
    pub result: String,
    pub runner_id: i64,
    pub runner_name: String,
    #[serde(flatten)]
    pub base: JobMessageBase,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Statistics {
    pub total_available_jobs: i64,
    pub total_acquired_jobs: i64,
    pub total_assigned_jobs: i64,
    pub total_running_jobs: i64,
    pub total_registered_runners: i64,
    pub total_busy_runners: i64,
    pub total_idle_runners: i64,
}

/// A decoded batch from the message queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScaleSetMessage {
    pub message_id: i64,
    pub statistics: Option<Statistics>,
    pub job_available: Vec<JobAvailable>,
    pub job_assigned: Vec<JobAssigned>,
    pub job_started: Vec<JobStarted>,
    pub job_completed: Vec<JobCompleted>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
}

impl Label {
    pub fn system(name: impl Into<String>) -> Self {
        Self { kind: "System".into(), name: name.into() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerSetting {
    #[serde(rename = "disableUpdate", default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_update: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerScaleSet {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub id: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub runner_group_id: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub runner_group_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<Label>,
    #[serde(rename = "RunnerSetting", default)]
    pub runner_setting: RunnerSetting,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statistics: Option<Statistics>,
}

fn is_zero(v: &i64) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RunnerGroup {
    pub id: i64,
    pub name: String,
    #[serde(rename = "isDefaultGroup", default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerReference {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub runner_scale_set_id: i64,
    /// Runner status. The agents API returns a string (`"online"`), while
    /// other endpoints return the numeric TaskAgentStatus (2 = online).
    #[serde(default)]
    pub status: serde_json::Value,
    #[serde(default)]
    pub busy: bool,
}

impl RunnerReference {
    pub fn is_online(&self) -> bool {
        match &self.status {
            serde_json::Value::String(s) => s.eq_ignore_ascii_case("online"),
            serde_json::Value::Number(n) => n.as_i64() == Some(2),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct JitRunnerConfig {
    pub runner: RunnerReference,
    #[serde(rename = "encodedJITConfig")]
    pub encoded_jit_config: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JitRunnerSetting<'a> {
    pub name: &'a str,
    pub work_folder: &'a str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner_name: String,
    #[serde(default, skip_serializing)]
    pub message_queue_url: String,
    #[serde(default, skip_serializing)]
    pub message_queue_access_token: String,
    #[serde(default, skip_serializing)]
    pub statistics: Option<Statistics>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ListResponse<T> {
    pub count: usize,
    #[serde(default = "Vec::new")]
    pub value: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RawMessage {
    pub message_id: i64,
    pub message_type: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub statistics: Option<Statistics>,
}

impl ScaleSetMessage {
    /// Decodes a queue message. The `body` is a JSON string containing an array
    /// of job messages; unknown job message types are ignored.
    pub(crate) fn decode(raw: RawMessage) -> Result<Self, String> {
        if raw.message_type != "RunnerScaleSetJobMessages" {
            return Err(format!("unsupported message type: {}", raw.message_type));
        }
        let mut msg = ScaleSetMessage { message_id: raw.message_id, statistics: raw.statistics, ..Default::default() };
        if raw.body.is_empty() {
            return Ok(msg);
        }
        let items: Vec<serde_json::Value> =
            serde_json::from_str(&raw.body).map_err(|e| format!("decoding batched messages: {e}"))?;
        for item in items {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Kind {
                message_type: MessageType,
            }
            let kind: Kind =
                serde_json::from_value(item.clone()).map_err(|e| format!("decoding job message type: {e}"))?;
            let err = |e: serde_json::Error| format!("decoding {:?}: {e}", kind.message_type);
            match kind.message_type {
                MessageType::JobAvailable => msg.job_available.push(serde_json::from_value(item).map_err(err)?),
                MessageType::JobAssigned => msg.job_assigned.push(serde_json::from_value(item).map_err(err)?),
                MessageType::JobStarted => msg.job_started.push(serde_json::from_value(item).map_err(err)?),
                MessageType::JobCompleted => msg.job_completed.push(serde_json::from_value(item).map_err(err)?),
                MessageType::Unknown => {}
            }
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_batched_job_messages() {
        let body = serde_json::json!([
            {"messageType":"JobAvailable","acquireJobUrl":"https://x/acquire","runnerRequestId":11,
             "repositoryName":"rgha","ownerName":"strawgate","jobWorkflowRef":"strawgate/rgha/.github/workflows/ci.yml@refs/heads/main",
             "eventName":"push","requestLabels":["rgha-small"],"queueTime":"2026-10-03T12:00:00Z"},
            {"messageType":"JobStarted","runnerRequestId":11,"runnerId":5,"runnerName":"r-1"},
            {"messageType":"JobCompleted","runnerRequestId":11,"runnerId":5,"runnerName":"r-1","result":"succeeded",
             "finishTime":"0001-01-01T00:00:00Z"},
            {"messageType":"SomethingNew","foo":1}
        ])
        .to_string();
        let raw = RawMessage {
            message_id: 42,
            message_type: "RunnerScaleSetJobMessages".into(),
            body,
            statistics: Some(Statistics { total_assigned_jobs: 3, ..Default::default() }),
        };
        let msg = ScaleSetMessage::decode(raw).unwrap();
        assert_eq!(msg.message_id, 42);
        assert_eq!(msg.statistics.unwrap().total_assigned_jobs, 3);
        assert_eq!(msg.job_available.len(), 1);
        assert_eq!(msg.job_available[0].base.event_name, "push");
        assert_eq!(msg.job_available[0].base.request_labels, vec!["rgha-small"]);
        assert_eq!(msg.job_started[0].runner_name, "r-1");
        assert_eq!(msg.job_completed[0].result, "succeeded");
    }

    #[test]
    fn runner_status_accepts_string_or_number() {
        let jit: JitRunnerConfig = serde_json::from_str(
            r#"{"runner":{"id":1,"name":"r","runnerScaleSetId":4,"status":0},"encodedJITConfig":"x"}"#,
        )
        .unwrap();
        assert!(!jit.runner.is_online());
        let r: RunnerReference = serde_json::from_str(r#"{"id":1,"name":"r","status":"online"}"#).unwrap();
        assert!(r.is_online());
        let r: RunnerReference = serde_json::from_str(r#"{"id":1,"name":"r","status":2}"#).unwrap();
        assert!(r.is_online());
    }

    #[test]
    fn rejects_unknown_envelope() {
        let raw = RawMessage { message_id: 1, message_type: "Nope".into(), body: String::new(), statistics: None };
        assert!(ScaleSetMessage::decode(raw).is_err());
    }

    #[test]
    fn scale_set_serializes_like_go_client() {
        let s = RunnerScaleSet {
            name: "rgha-small".into(),
            runner_group_id: 1,
            labels: vec![Label::system("rgha-small")],
            ..Default::default()
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["name"], "rgha-small");
        assert_eq!(v["runnerGroupId"], 1);
        assert_eq!(v["labels"][0]["type"], "System");
        assert!(v.get("id").is_none());
        assert!(v.get("RunnerSetting").is_some());
    }
}
