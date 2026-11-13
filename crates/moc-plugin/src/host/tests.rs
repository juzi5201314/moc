use super::*;

fn settings() -> Settings {
    Settings {
        call_timeout: Duration::from_millis(500),
        load_timeout: Duration::from_secs(2),
        ..Settings::default()
    }
}

fn install(cwd: &Path, name: &str, body: &str) {
    let dir = cwd.join(".moc/plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.json"), format!(r#"{{"name":"{name}"}}"#)).unwrap();
    std::fs::write(dir.join("index.js"), body).unwrap();
}

fn context() -> CallContext {
    CallContext {
        session_id: "s1".into(),
        cwd: "/tmp".into(),
        mode: "standard".into(),
        permission: "full".into(),
        model: "m".into(),
        is_subagent: false,
        writable: true,
    }
}

struct EchoBridge;

impl HostBridge for EchoBridge {
    fn call_tool<'a>(
        &'a self,
        name: String,
        args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
        Box::pin(async move {
            Ok(ToolResult {
                content: format!("{name}:{}", args["path"].as_str().unwrap_or_default()),
                structured: None,
                is_error: false,
            })
        })
    }
}

#[derive(Default)]
struct MemoryPersistence {
    config: Mutex<serde_json::Map<String, Value>>,
    state: Mutex<BTreeMap<(String, String, String), Value>>,
}

impl PersistenceBridge for MemoryPersistence {
    fn request<'a>(
        &'a self,
        plugin: &'a str,
        _path: &'a Path,
        context: Option<&'a CallContext>,
        op: PersistenceOp,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            match op {
                PersistenceOp::ConfigSnapshot => Ok(Value::Object(self.config.lock().clone())),
                PersistenceOp::ConfigGet { key } => Ok(match self.config.lock().get(&key) {
                    Some(value) => serde_json::json!({"found":true,"value":value}),
                    None => serde_json::json!({"found":false}),
                }),
                PersistenceOp::ConfigSet { key, value } => {
                    if value.is_null() {
                        self.config.lock().remove(&key);
                    } else {
                        self.config.lock().insert(key, value);
                    }
                    Ok(Value::Null)
                }
                op => {
                    let session = &context.expect("state 必须带会话").session_id;
                    let slot = |key: String| (plugin.to_owned(), session.clone(), key);
                    let mut state = self.state.lock();
                    Ok(match op {
                        PersistenceOp::StateSnapshot => Value::Object(
                            state
                                .iter()
                                .filter(|((name, id, _), _)| name == plugin && id == session)
                                .map(|((_, _, key), value)| (key.clone(), value.clone()))
                                .collect(),
                        ),
                        PersistenceOp::StateGet { key } => match state.get(&slot(key)) {
                            Some(value) => serde_json::json!({"found":true,"value":value}),
                            None => serde_json::json!({"found":false}),
                        },
                        PersistenceOp::StateSet { key, value } => {
                            state.insert(slot(key), value);
                            Value::Null
                        }
                        PersistenceOp::StateDelete { key } => {
                            Value::Bool(state.remove(&slot(key)).is_some())
                        }
                        PersistenceOp::StateKeys => serde_json::json!(
                            state
                                .keys()
                                .filter(|(name, id, _)| name == plugin && id == session)
                                .map(|(_, _, key)| key)
                                .collect::<Vec<_>>()
                        ),
                        _ => unreachable!(),
                    })
                }
            }
        })
    }
}

mod behavior;
mod hooks;
mod lifecycle;
mod state;
mod tools;

struct SlowBridge(Duration);

impl HostBridge for SlowBridge {
    fn call_tool<'a>(
        &'a self,
        _name: String,
        _args: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ToolResult>> + Send + 'a>> {
        Box::pin(async move {
            tokio::time::sleep(self.0).await;
            Ok(ToolResult {
                content: "slow".into(),
                structured: None,
                is_error: false,
            })
        })
    }
}
