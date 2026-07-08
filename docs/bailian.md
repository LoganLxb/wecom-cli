# 阿里百炼接入

`wecom-cli` 支持通过 `stdio MCP server` 模式接入阿里百炼。

## 启动方式

### 方式一：复用本地初始化

先完成一次企业微信机器人初始化：

```bash
wecom-cli init
```

然后启动 MCP Server：

```bash
wecom-cli mcp-server --transport stdio
```

### 方式二：使用环境变量

适合部署到服务器、容器或阿里百炼托管环境：

```bash
export WECOM_CLI_BOT_ID="your_bot_id"
export WECOM_CLI_BOT_SECRET="your_bot_secret"
wecom-cli mcp-server --transport stdio
```

## 百炼配置建议

- 启动命令：`wecom-cli mcp-server --transport stdio`
- 传输方式：`stdio`
- 凭证来源：优先使用环境变量；本地调试可复用 `wecom-cli init` 生成的加密配置

## 工具命名

对外暴露的工具名统一为 `<category>.<method>`：

```text
contact.get_userlist
doc.create_doc
meeting.get_meeting_info
msg.get_message
schedule.create_schedule
todo.add_todo
```

## 兼容性说明

- 会自动聚合 `contact/doc/meeting/msg/schedule/todo` 六个品类的远端工具。
- 会移除工具定义中的 `outputSchema`，避免严格校验平台因上游返回纯文本 JSON 而拒绝调用。
- 若工具返回 `content[].text` 中的 JSON 文本，会自动补充 `structuredContent`。
- `get_msg_media` 会继续下载媒体到本地临时目录，并在结果中返回 `local_path`。
