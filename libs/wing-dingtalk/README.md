# wing-dingtalk

Wing 钉钉前端 —— 通过钉钉与 Wing Agent 对话。

钉钉 Stream 模式（机器人出网建连，无需公网入口）收消息，经 `wing-sdk`
与 Gateway 通信：

- 收到消息秒回 emoji 回执；一轮 ReAct 结束（`turn_result`）后发送最终结果
- `ask` 事件逐题问答，`error` / `interrupted` 事件推送通知
- 前端命令：`/new` `/model` `/restart` `/sessions` `/switch` `/interrupt` `/help`
- 每个钉钉会话（conversation）各自维护当前 session，路由表落盘
- 注册 `SendFile` 远程工具，Agent 可把共享卷里的文件发到钉钉

## 用法

配置全部来自环境变量（见 `deploy/.env.example`），入口：

```bash
wing-dingtalk
```
