//! 运行真实检测插件进程，验证只读规则、统一分组复用、定向执行及清理与取消

use gateway_plugin_sdk::{
    CallContext, ErrorCode, Frame, Handshake, Manifest, Message, PluginFault, Stage,
    call::{
        host::ModelEventBatch,
        model::{CanonicalEvent, ExecutionEvent, FinishReason, WireEvent, WirePayload},
    },
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, ChildStdin, ChildStdout, Command},
    time::timeout,
};
type State = HashMap<String, (Value, u64)>;
/// 模拟宿主管理和模型回调，真实插件进程负责执行状态
struct Host {
    _child: Child,
    input: ChildStdin,
    output: ChildStdout,
    state: State,
    bodies: HashMap<String, VecDeque<Vec<u8>>>,
    id: u64,
    key_created: usize,
    managed_key_name: String,
    managed_key_label: String,
    selected_key: String,
    key_enabled: bool,
    key_group: String,
    group_calls: usize,
    model_calls: usize,
    closed: usize,
    failed_model: bool,
    denied: bool,
    account: String,
    primary_paths: Vec<String>,
}
impl Host {
    async fn start(state: State) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cpr-account-headers-test"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut host = Self {
            input: child.stdin.take().unwrap(),
            output: child.stdout.take().unwrap(),
            _child: child,
            state,
            bodies: HashMap::new(),
            id: 0,
            key_created: 0,
            managed_key_name: "请求头测试 test-instance group-auto".into(),
            managed_key_label: "cpr-account-headers:test-instance".into(),
            selected_key: "test-key".into(),
            key_enabled: true,
            key_group: "group-auto".into(),
            group_calls: 0,
            model_calls: 0,
            closed: 0,
            failed_model: false,
            denied: false,
            account: String::new(),
            primary_paths: Vec::new(),
        };
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        host.send(
            Message::Hello {
                handshake: Handshake {
                    protocol_version: 7,
                    artifact_sha256: "a".repeat(64),
                    plugin_id: manifest.plugin_id().unwrap(),
                    instance_id: "test-instance".into(),
                    generation: 1,
                    incarnation: "test".into(),
                    configuration: json!({}),
                    contributes: manifest.contributes,
                },
            },
            Vec::new(),
        )
        .await;
        assert!(matches!(
            host.read().await.message,
            Message::Ready {
                protocol_version: 7,
                ..
            }
        ));
        host
    }
    async fn send(&mut self, message: Message, payload: Vec<u8>) {
        write_frame(&mut self.input, &Frame { message, payload })
            .await
            .unwrap();
    }
    async fn read(&mut self) -> Frame {
        timeout(Duration::from_secs(10), read_frame(&mut self.output))
            .await
            .unwrap()
            .unwrap()
    }
    async fn request(
        &mut self,
        method: &str,
        path: &str,
        query: &str,
        body: Value,
    ) -> (u64, Value) {
        self.id += 1;
        let call_id = self.id * 2 - 1;
        self.send(Message::Call{id:call_id,method:"management.handle".into(),params:json!({"method":method,"path":path,"query":query,"content_type":if method=="POST"{Some("application/json")}else{None},"headers":[]}),context:CallContext{request:None,call_id,instance_id:"test-instance".into(),generation:1,incarnation:"test".into(),stage:Stage::Management,timeout_ms:30000,resource_stream:false,resource_scope_id:format!("call-{call_id}"),request_id:None,attempt_id:None,account_id:None,credential_revision:None}},if method=="POST"{serde_json::to_vec(&body).unwrap()}else{Vec::new()}).await;
        loop {
            let frame = self.read().await;
            match frame.message {
                Message::Result { id, result } if id == call_id => {
                    return (
                        result["status"].as_u64().unwrap(),
                        serde_json::from_slice(&frame.payload).unwrap(),
                    );
                }
                Message::Error { id, error } if id == call_id => {
                    panic!("管理调用失败 {}", error.message)
                }
                Message::Resume { id, epoch } => {
                    self.send(Message::Resumed { id, epoch }, Vec::new()).await
                }
                Message::Suspend { .. } | Message::CallbackConsumed { .. } => {}
                Message::Callback {
                    id,
                    parent_id: _,
                    method,
                    params,
                } => {
                    let mut payload = Vec::new();
                    let reply: Result<Value, PluginFault> = match method.as_str() {
                        "host.state.get" => {
                            let record=self.state.get(params["key"].as_str().unwrap()).map(|(value,version)|json!({"value":value,"version":version,"schema_version":1}));
                            Ok(json!({"record":record}))
                        }
                        "host.state.put" => {
                            let key = params["key"].as_str().unwrap().to_owned();
                            let current = self.state.get(&key).map(|(_, version)| *version);
                            if params["expected_version"] != json!(current) {
                                Err(PluginFault::new(ErrorCode::Conflict, "配置已更新"))
                            } else {
                                let version = current.unwrap_or(0) + 1;
                                self.state.insert(key, (params["value"].clone(), version));
                                Ok(json!({"version":version}))
                            }
                        }
                        "host.data.accounts.list" => {
                            payload=serde_json::to_vec(&json!({"schema_version":1,"accounts":[{"account_id":"account-a","provider_id":"openai","name":"账号 A","email":null,"group_ids":[],"enabled":true,"updated_at_ms":0},{"account_id":"account-b","provider_id":"xai","name":"账号 B","email":null,"group_ids":[],"enabled":true,"updated_at_ms":0}],"next_cursor":null})).unwrap();
                            Ok(json!({}))
                        }
                        "host.groups.ensure" => {
                            let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                            assert_eq!(q["resource_key"], "test.shared");
                            self.group_calls += 1;
                            payload = serde_json::to_vec(
                                &json!({"id":"group-auto","name":q["name"],"enabled":true}),
                            )
                            .unwrap();
                            Ok(json!({}))
                        }
                        "host.groups.change_members" => {
                            let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                            assert_eq!(q["add"], json!(["account-a", "account-b"]));
                            assert_eq!(q["remove"], json!([]));
                            payload = serde_json::to_vec(&json!({"added":2,"removed":0})).unwrap();
                            Ok(json!({}))
                        }
                        "host.data.keys.get" => {
                            let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                            self.selected_key = q["client_key_id"].as_str().unwrap().into();
                            let existing = self.selected_key != "test-key";
                            payload=serde_json::to_vec(&json!({"schema_version":1,"client_key_id":self.selected_key,"enabled":self.selected_key!="disabled-key"&&(!existing||self.key_enabled),"group_ids":if existing {json!([])}else{json!([self.key_group])}})).unwrap();
                            Ok(json!({}))
                        }
                        "host.data.access.check" => {
                            let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                            payload=serde_json::to_vec(&json!({"schema_version":1,"client_key_id":q["client_key_id"],"accounts":[{"account_id":q["account_ids"][0],"allowed":!self.denied}]})).unwrap();
                            Ok(json!({}))
                        }
                        "host.http.dispatch" => {
                            let uri = params["uri"].as_str().unwrap();
                            let data = if uri == "/api/admin/plugins/artifacts" {
                                json!({"code":200,"data":[{"metadata":{"pluginId":"silentflower.account-headers","sha256":"primary-sha"}}]})
                            } else if uri == "/api/admin/plugins/instances" {
                                json!({"code":200,"data":[{"id":"primary","artifactSha256":"primary-sha","revision":4,"enabled":true,"running":true}]})
                            } else if uri.starts_with("/api/admin/account-groups/options?") {
                                json!({"code":200,"data":{"items":[{"id":"existing","name":"已有分组","enabled":true}],"page":{"total":1}}})
                            } else if uri.starts_with("/api/admin/client-keys?") {
                                if !uri.contains("search=") {
                                    let next = uri.contains("cursor=keys-next");
                                    json!({"code":200,"data":{"items":if next {json!([{"id":"business-key","name":"openai-所有","enabled":self.key_enabled}])}else{json!([{"id":"disabled-key","name":"停用 Key","enabled":false}])},"nextCursor":if next {None}else{Some("keys-next")}}})
                                } else {
                                    json!({"code":200,"data":{"items":if self.key_created>0{json!([{"id":"test-key","name":self.managed_key_name,"label":self.managed_key_label,"enabled":true,"groups":[{"id":self.key_group}]}])}else{json!([])},"nextCursor":null}})
                                }
                            } else if uri == "/api/admin/client-keys/create" {
                                let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                                self.key_group = q["groupIds"][0].as_str().unwrap().into();
                                self.managed_key_name = q["name"].as_str().unwrap().into();
                                self.managed_key_label = q["label"].as_str().unwrap().into();
                                self.key_created += 1;
                                json!({"code":200,"data":{"id":"test-key","plaintextKey":"secret-must-stay-in-host"}})
                            } else if uri.starts_with("/api/admin/accounts/models?") {
                                json!({"code":200,"data":{"models":[{"id":"actual-model","label":"实际模型"}]}})
                            } else {
                                let path = uri
                                    .strip_prefix(
                                        "/api/admin/plugins/extensions/primary/primary-sha/4/api/",
                                    )
                                    .unwrap();
                                self.primary_paths.push(path.into());
                                if path == "diagnostics/status" {
                                    json!({"version":1})
                                } else if path.starts_with("accounts?") {
                                    json!({"accounts":[],"next_cursor":null})
                                } else if path.starts_with("rule?") {
                                    json!({"accountId":"account-a","enabled":true,"version":"1","headers":[{"name":"user-agent","value":"fixture"}]})
                                } else if path.starts_with("diagnostics/result?") {
                                    json!({"outbound":[{"accountId":self.account,"requestId":"request-real","attemptIndex":1,"transport":"http","overrideEnabled":true,"beforeHeaders":[],"headers":[{"name":"user-agent","value":"fixture"}]}]})
                                } else {
                                    assert!(matches!(
                                        path,
                                        "diagnostics/register"
                                            | "diagnostics/finish"
                                            | "diagnostics/cancel"
                                    ));
                                    json!({"ended":true})
                                }
                            };
                            let handle = format!("http-{id}");
                            self.bodies.insert(
                                handle.clone(),
                                VecDeque::from([serde_json::to_vec(&data).unwrap()]),
                            );
                            Ok(
                                json!({"status":200,"version":"HTTP/1.1","headers":[],"body":{"kind":"handle","handle":handle},"response":null,"session":false}),
                            )
                        }
                        "host.http.body_read" => {
                            let bytes = self
                                .bodies
                                .get_mut(params["handle"].as_str().unwrap())
                                .unwrap()
                                .pop_front();
                            let eof = bytes.is_none();
                            payload = bytes.unwrap_or_default();
                            Ok(json!({"eof":eof,"trailers":null}))
                        }
                        "host.http.body_close" => Ok(json!({})),
                        "host.model.execute_stream" => {
                            self.model_calls += 1;
                            self.account = params["account_id"].as_str().unwrap().into();
                            assert_eq!(params["client_key_id"], self.selected_key);
                            assert_eq!(
                                params["provider"],
                                if self.account == "account-b" {
                                    "xai"
                                } else {
                                    "openai"
                                }
                            );
                            let q: Value = serde_json::from_slice(&frame.payload).unwrap();
                            assert!(q["metadata"]["cpr_account_headers_test"].as_str().is_some());
                            Ok(json!({"request_id":"request-real","stream":"model-stream"}))
                        }
                        "host.model.stream_read" => {
                            let events = if self.failed_model {
                                vec![ExecutionEvent::wire(WireEvent {
                                    protocol: "openai".into(),
                                    payload: WirePayload::Json {
                                        event: None,
                                        data: json!({"type":"error","error":{"message":"合成上游错误"}}),
                                        id: None,
                                        retry: None,
                                        raw_sse: None,
                                    },
                                })]
                            } else if self.account == "account-b" {
                                vec![ExecutionEvent::wire(WireEvent {protocol:"openai".into(),payload:WirePayload::RawSse {frame:b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"OK\"}]}]}}\n\n".to_vec()}})]
                            } else {
                                vec![
                                    ExecutionEvent::canonical(CanonicalEvent::TextDelta {
                                        index: 0,
                                        text: "OK".into(),
                                    }),
                                    ExecutionEvent::canonical(CanonicalEvent::Completed {
                                        id: "response".into(),
                                        model: Some("actual-model".into()),
                                        reason: FinishReason::Stop,
                                    }),
                                ]
                            };
                            let count = events.len();
                            payload = ModelEventBatch { events }.encode().unwrap();
                            Ok(json!({"events":count,"end":true}))
                        }
                        "host.model.stream_close" => {
                            self.closed += 1;
                            Ok(json!({}))
                        }
                        other => panic!("未预期的宿主回调 {other}"),
                    };
                    match reply {
                        Ok(result) => self.send(Message::Result { id, result }, payload).await,
                        Err(error) => self.send(Message::Error { id, error }, Vec::new()).await,
                    }
                }
                other => panic!("未预期的宿主消息 {other:?}"),
            }
        }
    }
}
#[tokio::test]
async fn readonly_rules_and_models_have_no_rule_mutation_route() {
    let mut host = Host::start(State::new()).await;
    assert_eq!(
        host.request("GET", "accounts", "", Value::Null).await.0,
        200
    );
    assert_eq!(
        host.request("GET", "rule", "accountId=account-a", Value::Null)
            .await
            .1["headers"][0]["value"],
        "fixture"
    );
    assert_eq!(host.request("POST", "rule", "", json!({})).await.0, 400);
    assert_eq!(
        host.request("GET", "models", "accountId=account-a", Value::Null)
            .await
            .1["models"][0]["id"],
        "actual-model"
    );
    assert!(
        host.primary_paths
            .iter()
            .all(|p| p.starts_with("accounts?") || p.starts_with("rule?"))
    );
}
#[tokio::test]
async fn shared_group_reuses_key_and_pins_each_provider_with_real_evidence() {
    let mut host = Host::start(State::new()).await;
    for (account, id) in [("account-a", "a"), ("account-b", "b"), ("account-a", "c")] {
        let (status, value) = host
            .request(
                "POST",
                "test",
                "",
                json!({"id":id.repeat(32),"accountId":account,"modelId":"actual-model"}),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(value["status"], "success");
        assert_eq!(value["text"], "OK");
        assert_eq!(value["outbound"][0]["accountId"], account);
    }
    assert_eq!(host.key_created, 1);
    assert_eq!(host.closed, 3);
    assert_eq!(host.model_calls, 3);
    let mut legacy = Host::start(State::new()).await;
    legacy.key_created = 1;
    assert_eq!(
        legacy
            .request(
                "POST",
                "test",
                "",
                json!({"id":"f".repeat(32),"accountId":"account-a","modelId":"actual-model"})
            )
            .await
            .1["status"],
        "success"
    );
    assert_eq!(legacy.key_created, 1, "旧标签资源继续复用");

    assert!(
        !host
            .primary_paths
            .iter()
            .any(|p| p.starts_with("rule?") || p == "rule")
    );
}
#[tokio::test]
async fn group_settings_survive_restart_and_existing_groups_keep_members() {
    let mut host = Host::start(State::new()).await;
    let (status, saved) = host
        .request(
            "POST",
            "test/settings",
            "",
            json!({"groupId":"existing","version":null}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(saved["version"], "1");
    assert_eq!(
        host.request(
            "POST",
            "test/settings",
            "",
            json!({"groupId":null,"version":null})
        )
        .await
        .0,
        409
    );
    let mut host = Host::start(host.state.clone()).await;
    assert_eq!(
        host.request("GET", "test/settings", "", Value::Null)
            .await
            .1["settings"]["groupId"],
        "existing"
    );
    assert_eq!(
        host.request(
            "POST",
            "test",
            "",
            json!({"id":"a".repeat(32),"accountId":"account-a","modelId":"actual-model"})
        )
        .await
        .1["status"],
        "success"
    );
    assert_eq!(host.key_group, "existing");
    assert_eq!(host.group_calls, 0);
}
#[tokio::test]
async fn cancelled_denied_and_failed_tests_keep_model_resources_bounded() {
    let mut host = Host::start(State::new()).await;
    let id = "a".repeat(32);
    host.request("POST", "test/cancel", "", json!({"id":id}))
        .await;
    assert_eq!(
        host.request(
            "POST",
            "test",
            "",
            json!({"id":id,"accountId":"account-a","modelId":"actual-model"})
        )
        .await
        .0,
        409
    );
    assert_eq!(host.model_calls, 0);
    host.denied = true;
    assert_eq!(
        host.request(
            "POST",
            "test",
            "",
            json!({"id":"b".repeat(32),"accountId":"account-a","modelId":"actual-model"})
        )
        .await
        .0,
        400
    );
    assert_eq!(host.model_calls, 0);
    host.denied = false;
    host.failed_model = true;
    let (status, value) = host
        .request(
            "POST",
            "test",
            "",
            json!({"id":"c".repeat(32),"accountId":"account-a","modelId":"actual-model"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(value["status"], "failed");
    assert_eq!(value["message"], "合成上游错误");
    assert_eq!(host.closed, 1);
    assert!(host.primary_paths.contains(&"diagnostics/finish".into()));
}

#[tokio::test]
async fn existing_key_uses_its_scope_without_creating_resources_and_reads_legacy_settings() {
    let mut host = Host::start(State::from([(
        "settings".into(),
        (json!({"groupId":"existing"}), 7),
    )]))
    .await;
    let (status, value) = host.request("GET", "test/settings", "", Value::Null).await;
    assert_eq!(status, 200);
    assert!(value["settings"]["keyId"].is_null());
    assert_eq!(value["settings"]["groupId"], "existing");
    assert_eq!(value["settings"]["version"], "7");
    assert!(
        value["keys"]
            .as_array()
            .unwrap()
            .iter()
            .any(|key| key["name"] == "openai-所有")
    );
    let (status, saved) = host
        .request(
            "POST",
            "test/settings",
            "",
            json!({"keyId":"business-key","groupId":"existing","version":"7"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(saved["keyId"], "business-key");
    let (status, result) = host
        .request(
            "POST",
            "test",
            "",
            json!({"id":"d".repeat(32),"accountId":"account-b","modelId":"actual-model"}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(result["status"], "success");
    assert_eq!(host.selected_key, "business-key");
    assert_eq!(host.key_created, 0);
    assert_eq!(host.group_calls, 0);
    assert_eq!(host.closed, 1);
    assert_eq!(
        host.request(
            "POST",
            "test/settings",
            "",
            json!({"keyId":"disabled-key","groupId":null,"version":saved["version"]})
        )
        .await
        .0,
        400
    );
    host.denied = true;
    assert_eq!(
        host.request(
            "POST",
            "test",
            "",
            json!({"id":"e".repeat(32),"accountId":"account-a","modelId":"actual-model"})
        )
        .await
        .0,
        400
    );
    host.denied = false;
    host.key_enabled = false;
    assert_eq!(
        host.request(
            "POST",
            "test",
            "",
            json!({"id":"f".repeat(32),"accountId":"account-a","modelId":"actual-model"})
        )
        .await
        .0,
        400
    );
    assert_eq!(host.model_calls, 1);
    assert_eq!(host.key_created, 0);
    assert_eq!(host.group_calls, 0);
}
