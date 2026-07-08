# wing.gateway — Gateway 传输层 (FastAPI)
#
# Gateway 是 wing 的网络前端，基于 FastAPI + uvicorn 构建。
# 提供 WebSocket + HTTP 两种协议访问。
#
# 模块结构：
#   app.py        — FastAPI App 工厂
#   server.py     — GatewayServer 生命周期管理 + EventBus 路由
#   cli.py        — CLI 入口 (wing-gateway)
#   protocol.py   — WS + HTTP 消息协议 (Pydantic models)
#   openapi.py    — OpenAPI tags 元数据
#   routes/       — HTTP 路由 (session, health, ws)
