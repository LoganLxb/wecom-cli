use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use anyhow::Result;
use clap::{ArgMatches, Args, Command, FromArgMatches};
use serde_json::{Map, Value, json};

use crate::{json_rpc, media, registry, service};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Args, Debug)]
pub struct McpServerArgs {
    /// 传输协议，目前仅支持 stdio
    #[arg(long, default_value = "stdio")]
    pub transport: String,
}

pub fn build_mcp_server_cmd() -> Command {
    McpServerArgs::augment_args(
        Command::new("mcp-server")
            .visible_alias("bailian-mcp")
            .about("以 stdio MCP Server 模式启动，供阿里百炼等平台接入"),
    )
    .disable_help_flag(true)
}

pub async fn handle_mcp_server_cmd(matches: &ArgMatches) -> Result<()> {
    let args = McpServerArgs::from_arg_matches(matches)?;

    if args.transport != "stdio" {
        anyhow::bail!("当前仅支持 --transport stdio");
    }

    run_stdio_server().await
}

async fn run_stdio_server() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some(message) = read_message(&mut reader)? {
        if let Some(response) = handle_message(message).await {
            write_message(&mut writer, &response)?;
        }
    }

    Ok(())
}

async fn handle_message(message: Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str);
    let params = message.get("params");

    let response = match method {
        Some("initialize") => Ok(build_initialize_result(params)),
        Some("notifications/initialized") => return None,
        Some("ping") => Ok(json!({})),
        Some("tools/list") => handle_tools_list().await,
        Some("tools/call") => handle_tools_call(params).await,
        Some(other) => Err(anyhow::anyhow!("未知方法: {other}")),
        None => Err(anyhow::anyhow!("无效请求：缺少 method")),
    };

    let id = match id {
        Some(id) => id,
        None => return None,
    };

    Some(match response {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": err.to_string(),
            }
        }),
    })
}

fn build_initialize_result(params: Option<&Value>) -> Value {
    let protocol_version = params
        .and_then(|value| value.get("protocolVersion"))
        .and_then(Value::as_str)
        .unwrap_or(MCP_PROTOCOL_VERSION);

    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "wecom-cli-bailian-mcp",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": format!(
            "在阿里百炼中使用企业微信工具。请先运行 `{} init`，或设置环境变量 {} 和 {} 后再启动服务。",
            env!("CARGO_BIN_NAME"),
            crate::constants::env::BOT_ID,
            crate::constants::env::BOT_SECRET
        )
    })
}

async fn handle_tools_list() -> Result<Value> {
    let mut tools = Vec::new();

    for category in service::categories::get_categories() {
        let category_tools = registry::get_category_tools(category.name).await?;
        for tool in category_tools {
            tools.push(sanitize_tool_definition(category.name, &tool)?);
        }
    }

    Ok(json!({ "tools": tools }))
}

async fn handle_tools_call(params: Option<&Value>) -> Result<Value> {
    let params = params.ok_or_else(|| anyhow::anyhow!("缺少 params"))?;
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("缺少工具名 params.name"))?;

    let (category, method) = decode_tool_name(name)
        .ok_or_else(|| anyhow::anyhow!("非法工具名 `{name}`，期望 `<category>.<method>`"))?;

    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let timeout_ms = if method == "get_msg_media" {
        Some(120000)
    } else {
        None
    };

    let upstream = json_rpc::send(
        category,
        "tools/call",
        Some(json!({
            "name": method,
            "arguments": arguments,
        })),
        timeout_ms,
    )
    .await;

    let upstream = match upstream {
        Ok(response) => response,
        Err(err) => return Ok(build_error_tool_result(err.to_string())),
    };

    let upstream = if method == "get_msg_media" {
        match media::intercept_media_response(upstream).await {
            Ok(response) => response,
            Err(err) => return Ok(build_error_tool_result(err.to_string())),
        }
    } else {
        upstream
    };

    Ok(normalize_tool_result(upstream))
}

fn sanitize_tool_definition(category: &str, tool: &registry::ServiceTool) -> Result<Value> {
    let mut value = serde_json::to_value(tool)?;
    let obj = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("工具定义不是对象"))?;

    obj.insert(
        "name".to_string(),
        Value::String(encode_tool_name(category, &tool.name)),
    );
    obj.remove("outputSchema");

    if let Some(description) = obj.get("description").and_then(Value::as_str) {
        obj.insert(
            "description".to_string(),
            Value::String(format!("[{category}] {description}")),
        );
    }

    Ok(value)
}

fn normalize_tool_result(response: Value) -> Value {
    let Some(result) = response.get("result").and_then(Value::as_object) else {
        return build_error_tool_result(format!("上游工具返回格式异常：{response}"));
    };

    let mut result = result.clone();
    if !result.contains_key("isError") {
        result.insert("isError".to_string(), Value::Bool(false));
    }

    if !result.contains_key("structuredContent") {
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            if let Some(structured) = extract_structured_content(content) {
                result.insert("structuredContent".to_string(), structured);
            }
        }
    }

    Value::Object(result)
}

fn extract_structured_content(content: &[Value]) -> Option<Value> {
    let text = content.iter().find_map(|item| {
        (item.get("type").and_then(Value::as_str) == Some("text"))
            .then(|| item.get("text").and_then(Value::as_str))
            .flatten()
    })?;

    serde_json::from_str(text).ok()
}

fn build_error_tool_result(message: String) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": message,
        }],
        "isError": true,
    })
}

fn encode_tool_name(category: &str, method: &str) -> String {
    format!("{category}.{method}")
}

fn decode_tool_name(name: &str) -> Option<(&str, &str)> {
    let (category, method) = name.split_once('.')?;
    if category.is_empty() || method.is_empty() {
        return None;
    }
    Some((category, method))
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;

        if read == 0 {
            return Ok(None);
        }

        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }

        if trimmed.starts_with('{') {
            return Ok(Some(serde_json::from_str(trimmed)?));
        }

        if let Some((name, value)) = trimmed.split_once(':') {
            if name.eq_ignore_ascii_case("Content-Length") {
                content_length = Some(value.trim().parse()?);
            }
        }
    }

    let Some(content_length) = content_length else {
        return Ok(None);
    };

    let mut body = vec![0_u8; content_length];
    reader.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message)?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_tool_name_requires_separator() {
        assert_eq!(decode_tool_name("contact.get_userlist"), Some(("contact", "get_userlist")));
        assert_eq!(decode_tool_name("contact"), None);
        assert_eq!(decode_tool_name(".get_userlist"), None);
        assert_eq!(decode_tool_name("contact."), None);
    }

    #[test]
    fn extract_structured_content_parses_json_text() {
        let content = vec![json!({
            "type": "text",
            "text": r#"{"errcode":0,"errmsg":"ok"}"#
        })];

        assert_eq!(
            extract_structured_content(&content),
            Some(json!({"errcode": 0, "errmsg": "ok"}))
        );
    }

    #[test]
    fn normalize_tool_result_adds_structured_content() {
        let normalized = normalize_tool_result(json!({
            "result": {
                "content": [{
                    "type": "text",
                    "text": r#"{"errcode":0}"#
                }]
            }
        }));

        assert_eq!(
            normalized,
            json!({
                "content": [{
                    "type": "text",
                    "text": r#"{"errcode":0}"#
                }],
                "structuredContent": {
                    "errcode": 0
                },
                "isError": false
            })
        );
    }

    #[test]
    fn normalize_tool_result_reports_invalid_shape() {
        let normalized = normalize_tool_result(json!({"foo": "bar"}));
        assert_eq!(normalized.get("isError").and_then(Value::as_bool), Some(true));
    }

    #[test]
    fn read_message_supports_json_line_mode() {
        let mut reader = BufReader::new(r#"{"jsonrpc":"2.0","method":"ping"}"#.as_bytes());
        let message = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message["method"], "ping");
    }

    #[test]
    fn read_message_supports_content_length_mode() {
        let payload = r#"{"jsonrpc":"2.0","method":"ping"}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", payload.len(), payload);
        let mut reader = BufReader::new(raw.as_bytes());
        let message = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(message["method"], "ping");
    }
}
