//! End-to-end protocol flow against a mock GitHub + Actions service.

use base64::Engine;
use rgha_scaleset::{Client, ClientOptions, Credentials, Error};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn fake_jwt(exp: i64) -> String {
    let enc = |s: String| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    format!("{}.{}.sig", enc(r#"{"alg":"none"}"#.into()), enc(format!(r#"{{"exp":{exp}}}"#)))
}

async fn setup() -> (MockServer, Client) {
    let server = MockServer::start().await;
    let exp = chrono::Utc::now().timestamp() + 3600;

    Mock::given(method("POST"))
        .and(path("/repos/strawgate/rgha/actions/runners/registration-token"))
        .and(header("authorization", "Bearer pat-123"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"token": "reg-tok"})))
        .expect(1) // admin token is cached across calls
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/actions/runner-registration"))
        .and(header("authorization", "RemoteAuth reg-tok"))
        .and(body_json(json!({"url": "https://github.com/strawgate/rgha", "runner_event": "register"})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"url": format!("{}/tenant/", server.uri()), "token": fake_jwt(exp)})),
        )
        .mount(&server)
        .await;

    let opts = ClientOptions { api_base_url: Some(server.uri()), max_retries: 1, ..Default::default() };
    let client = Client::new("https://github.com/strawgate/rgha", Credentials::Token("pat-123".into()), opts).unwrap();
    (server, client)
}

#[tokio::test]
async fn full_listener_protocol_flow() {
    let (server, client) = setup().await;
    Mock::given(method("GET"))
        .and(path("/tenant/_apis/runtime/runnergroups/"))
        .and(query_param("groupName", "default"))
        .and(query_param("api-version", "6.0-preview"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"count":1,"value":[{"id":1,"name":"default","isDefaultGroup":true}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/tenant/_apis/runtime/runnerscalesets"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"count":0,"value":[]})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tenant/_apis/runtime/runnerscalesets"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["labels"][0]["name"], "rgha-small");
            assert_eq!(body["labels"][0]["type"], "System");
            ResponseTemplate::new(200)
                .set_body_json(json!({"id": 7, "name": body["name"], "runnerGroupId": 1, "labels": body["labels"]}))
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tenant/_apis/runtime/runnerscalesets/7/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionId": "8a7c4ad0-7c8e-4a1b-9f3e-5b1b2c3d4e5f",
            "ownerName": "rgha-test",
            "messageQueueUrl": format!("{}/queue/messages", server.uri()),
            "messageQueueAccessToken": "queue-tok",
            "statistics": {"totalAssignedJobs": 0}
        })))
        .mount(&server)
        .await;
    let body = json!([{"messageType":"JobAvailable","runnerRequestId":99,"eventName":"push",
                       "jobWorkflowRef":"strawgate/rgha/.github/workflows/ci.yml@refs/heads/main"}])
    .to_string();
    Mock::given(method("GET"))
        .and(path("/queue/messages"))
        .and(header("authorization", "Bearer queue-tok"))
        .and(header("x-scalesetmaxcapacity", "5"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "messageId": 3, "messageType": "RunnerScaleSetJobMessages", "body": body,
            "statistics": {"totalAssignedJobs": 1}
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/queue/messages"))
        .and(query_param("lastMessageId", "3"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tenant/_apis/runtime/runnerscalesets/7/acquirejobs"))
        .and(body_json(json!([99])))
        .respond_with(|req: &Request| {
            // Exactly one Authorization header, and it must be the queue token.
            let auths: Vec<_> = req.headers.get_all("authorization").iter().collect();
            assert_eq!(auths.len(), 1);
            assert_eq!(auths[0], "Bearer queue-tok");
            ResponseTemplate::new(200).set_body_json(json!({"count":1,"value":[99]}))
        })
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/queue/messages/3"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/tenant/_apis/runtime/runnerscalesets/7/generatejitconfig"))
        .and(body_json(json!({"name": "rgha-small-abc", "workFolder": "_work"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "runner": {"id": 501, "name": "rgha-small-abc", "runnerScaleSetId": 7},
            "encodedJITConfig": "ZW5jb2RlZA=="
        })))
        .mount(&server)
        .await;

    let group = client.get_runner_group_by_name("default").await.unwrap();
    assert_eq!(group.id, 1);
    assert!(client.get_scale_set(group.id, "rgha-small").await.unwrap().is_none());
    let ss = client
        .create_scale_set(rgha_scaleset::RunnerScaleSet {
            name: "rgha-small".into(),
            runner_group_id: 1,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(ss.id, 7);

    let session = client.message_session(ss.id, "rgha-test").await.unwrap();
    let msg = session.get_message(0, 5).await.unwrap().expect("a message");
    assert_eq!(msg.message_id, 3);
    assert_eq!(msg.job_available[0].base.runner_request_id, 99);
    assert_eq!(session.acquire_jobs(&[99]).await.unwrap(), vec![99]);
    session.delete_message(3).await.unwrap();
    assert!(session.get_message(3, 5).await.unwrap().is_none());

    let jit = client.generate_jit_config(ss.id, "rgha-small-abc", "_work").await.unwrap();
    assert_eq!(jit.runner.id, 501);
    assert_eq!(jit.encoded_jit_config, "ZW5jb2RlZA==");
}

#[tokio::test]
async fn expired_queue_token_refreshes_session_once() {
    let (server, client) = setup().await;
    Mock::given(method("POST"))
        .and(path("/tenant/_apis/runtime/runnerscalesets/7/sessions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionId": "8a7c4ad0-7c8e-4a1b-9f3e-5b1b2c3d4e5f",
            "messageQueueUrl": format!("{}/queue/messages", server.uri()),
            "messageQueueAccessToken": "old", "statistics": {}
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/queue/messages"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/tenant/_apis/runtime/runnerscalesets/7/sessions/8a7c4ad0-7c8e-4a1b-9f3e-5b1b2c3d4e5f"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessionId": "8a7c4ad0-7c8e-4a1b-9f3e-5b1b2c3d4e5f",
            "messageQueueUrl": format!("{}/queue/messages", server.uri()),
            "messageQueueAccessToken": "new", "statistics": {}
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/queue/messages"))
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(202))
        .mount(&server)
        .await;

    let session = client.message_session(7, "o").await.unwrap();
    assert!(session.get_message(0, 1).await.unwrap().is_none());
    assert_eq!(session.session().await.message_queue_access_token, "new");
}

#[tokio::test]
async fn http_errors_surface_status_and_message() {
    let (server, client) = setup().await;
    Mock::given(method("DELETE"))
        .and(path("/tenant/_apis/distributedtask/pools/0/agents/5"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"message": "runner is busy"})))
        .mount(&server)
        .await;
    let err = client.remove_runner(5).await.unwrap_err();
    assert_eq!(err.status(), Some(400));
    assert!(matches!(&err, Error::Http { message, .. } if message == "runner is busy"));
}
