# PyO3 / Python 互操作边界（P2-7）

## 结论

| 路径 | 何时用 | 如何构建 |
|------|--------|----------|
| **HTTP MCP（推荐生产）** | agent-core / Cursor / Reasonix / 任意 MCP 客户端 | `cargo build --release`（默认 **无** PyO3） |
| **PyO3 扩展模块** | 仅当需在同一进程内 `import memoria_core` | `cargo build --release --features python` |

**禁止**：一边用 `MemoriaEngine`（PyO3）写库，一边再跑 `memoria-server` 写同一 `memoria.db`（双写）。单写者 PID 锁会挡住第二进程；仍应只选一条路径。

## 默认为何关闭 python

历史上 `default = ["python"]` 会把 PyO3 链进二进制，在部分 Windows 环境会拉起解释器相关线程，加重 CPU 空转风险。生产入口是独立 `memoria-server`，不需要扩展模块。

## 快速对照

```bash
# 生产（默认）
cargo build --release
./target/release/memoria-server

# 仅开发 / 嵌入 Python
cargo build --release --features python
```

环境变量与 MCP 配置见 `.env.example` 与 `examples/`。
