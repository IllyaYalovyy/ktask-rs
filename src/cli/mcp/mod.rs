//! Local JSON-RPC stdio transport serving every project of the instance.
mod tools;

use serde_json::{Map, Value, json};
use std::io::{self, BufRead, Read, Write};
pub(super) use tools::Host;
use tools::Tool;

const MAX_FRAME_BYTES: u64 = 1024 * 1024;
const PROTOCOL_VERSIONS: [&str; 2] = ["2025-11-25", "2025-06-18"];

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const NOT_INITIALIZED: i32 = -32002;

pub(super) fn serve(host: Host) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    let mut server = Server {
        host,
        initialized: false,
    };
    let mut frame = Vec::new();

    while input
        .by_ref()
        .take(MAX_FRAME_BYTES + 1)
        .read_until(b'\n', &mut frame)?
        != 0
    {
        let response = if frame.len() as u64 > MAX_FRAME_BYTES {
            if frame.last() != Some(&b'\n') {
                input.skip_until(b'\n')?;
            }
            Some(
                RpcError(PARSE_ERROR, "Message exceeds the 1 MiB frame limit".into())
                    .response(&Value::Null),
            )
        } else {
            match serde_json::from_slice(&frame) {
                Ok(message) => server.message(&message),
                Err(_) => Some(RpcError(PARSE_ERROR, "Invalid JSON".into()).response(&Value::Null)),
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
        frame.clear();
    }
    Ok(())
}

struct RpcError(i32, String);

impl RpcError {
    fn params(message: impl Into<String>) -> Self {
        Self(INVALID_PARAMS, message.into())
    }

    fn response(self, id: &Value) -> Value {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":self.0,"message":self.1}})
    }
}

struct Server {
    host: Host,
    initialized: bool,
}

impl Server {
    fn message(&mut self, message: &Value) -> Option<Value> {
        let id = message.get("id");
        let valid_id = id.filter(|id| id.is_string() || id.is_i64() || id.is_u64());
        let response_fields = message.get("result").is_some() || message.get("error").is_some();
        let method = message.get("method");
        let version = message.get("jsonrpc").and_then(Value::as_str);
        if version == Some("2.0") && method.is_none() && response_fields && valid_id.is_some() {
            return None;
        }
        let valid_method = method
            .and_then(Value::as_str)
            .filter(|method| !method.is_empty());
        if version != Some("2.0") || valid_method.is_none() || response_fields || id != valid_id {
            return Some(
                RpcError(INVALID_REQUEST, "Invalid JSON-RPC request".into())
                    .response(valid_id.unwrap_or(&Value::Null)),
            );
        }
        // This server makes no requests of clients and performs no work for notifications.
        let id = valid_id?;
        let method = valid_method?;
        let empty = Map::new();
        let params = match message.get("params") {
            Some(Value::Object(params)) => params,
            Some(_) => return Some(RpcError::params("params must be an object").response(id)),
            None => &empty,
        };
        if params.get("_meta").is_some_and(|meta| !meta.is_object()) {
            return Some(RpcError::params("_meta must be an object").response(id));
        }
        Some(match self.request(method, params) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => error.response(id),
        })
    }

    fn request(&mut self, method: &str, params: &Map<String, Value>) -> Result<Value, RpcError> {
        match method {
            "initialize" => self.initialize(params),
            "ping" => Ok(json!({})),
            "tools/list" | "tools/call" if !self.initialized => Err(RpcError(
                NOT_INITIALIZED,
                "Initialize the MCP server first".into(),
            )),
            "tools/list" => {
                if params.keys().any(|key| key != "_meta") {
                    return Err(RpcError::params(
                        "The tool list has a single page and takes no cursor",
                    ));
                }
                Ok(json!({"tools":Tool::ALL.map(Tool::spec)}))
            }
            "tools/call" => self.call_tool(params),
            _ => Err(RpcError(METHOD_NOT_FOUND, "Method not found".into())),
        }
    }

    fn initialize(&mut self, params: &Map<String, Value>) -> Result<Value, RpcError> {
        if self.initialized {
            return Err(RpcError(
                INVALID_REQUEST,
                "MCP server is already initialized".into(),
            ));
        }
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .ok_or_else(|| RpcError::params("initialize requires protocolVersion"))?;
        let client = params.get("clientInfo").and_then(Value::as_object);
        if !params.get("capabilities").is_some_and(Value::is_object)
            || !client.is_some_and(|client| {
                ["name", "version"]
                    .iter()
                    .all(|key| client.get(*key).is_some_and(Value::is_string))
            })
        {
            return Err(RpcError::params(
                "initialize requires capabilities and clientInfo with name and version",
            ));
        }
        let version = PROTOCOL_VERSIONS
            .into_iter()
            .find(|version| *version == requested)
            .unwrap_or(PROTOCOL_VERSIONS[0]);
        self.initialized = true;
        Ok(json!({
            "protocolVersion":version,
            "capabilities":{"tools":{"listChanged":false}},
            "serverInfo":{"name":"ktask","title":crate::PRODUCT,"version":self.host.version},
        }))
    }

    fn call_tool(&mut self, params: &Map<String, Value>) -> Result<Value, RpcError> {
        if params
            .keys()
            .any(|key| !matches!(key.as_str(), "name" | "arguments" | "_meta"))
        {
            return Err(RpcError::params("Unexpected tools/call parameter"));
        }
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::params("tools/call requires a tool name"))?;
        let tool = Tool::ALL
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| RpcError::params("Unknown tool"))?;
        let default = json!({});
        let arguments = params.get("arguments").unwrap_or(&default);
        if !arguments.is_object() {
            return Err(RpcError::params("arguments must be an object"));
        }
        Ok(match tool.invoke(&self.host, arguments) {
            Ok(data) => {
                json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data})
            }
            Err(error) => {
                json!({"content":[{"type":"text","text":format!("{}: {}",error.code,error.message)}],"isError":true})
            }
        })
    }
}
