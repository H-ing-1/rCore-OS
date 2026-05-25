# rCore-OS VirtIO Sound Driver

面向“开源社区唐图 rCore-OS”赛道的 `VirtIO Sound` 驱动原型仓库。

我围绕 `virtio-snd` 设备支持完成了这个 Rust 驱动原型，实现了 `VirtQueue` 描述符环管理、PCM 生命周期控制、播放数据提交、音量与静音控制、通道映射查询、缓冲规划、信号分析和大规模工作负载目录，并补齐了单元测试与 CI 入口。仓库采用标准 Rust 项目结构，源码集中在 `src/` 目录，便于评审直接核查实现细节。

## 核心功能

1. `VirtQueue` 描述符链分配、提交、完成回收与中断标志管理。
2. PCM 流状态机控制：`SetParameters -> Prepare -> Start -> Stop -> Release`。
3. 音频播放链路：向 TX 队列提交 PCM 帧并处理完成项。
4. 控制面功能：静音、音量、全局静音、PCM 信息查询、通道映射查询。

## 赛道符合性

| 要求 | 当前实现 |
| --- | --- |
| 1000+ 行有效代码 | 当前 Rust 源码约 `17782` 行非空、非注释代码 |
| 至少 2 项独立功能 | 当前已具备 7 项独立功能 |
| 自动化测试能力 | 内置五十余个单元测试，并提供 GitHub Actions |
| 文档完整 | README、架构、审计、推荐表草稿齐备 |
| 开源仓库可持续更新 | 已连接 GitHub 远程仓库，可直接 commit/push |

## 仓库结构

```text
rCore-OS/
├─ .github/workflows/rust.yml
├─ docs/
│  ├─ ARCHITECTURE.md
│  ├─ CODE_AUDIT.md
│  └─ MIDTERM_SUMMARY.md
├─ examples/
│  ├─ basic_lifecycle.rs
│  └─ control_and_queue.rs
├─ src/
│  ├─ audio_planner.rs
│  ├─ lib.rs
│  ├─ signal_analysis.rs
│  └─ workload_catalog.rs
├─ Cargo.toml
├─ LICENSE
└─ README.md
```

## 快速开始

```bash
git clone git@github.com:H-ing-1/rCore-OS.git
cd rCore-OS
cargo test
```

如果需要静态检查：

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

## 设计说明

- 驱动主体位于 [src/lib.rs](./src/lib.rs)。
- 缓冲规划与调度策略位于 [src/audio_planner.rs](./src/audio_planner.rs)。
- 信号分析工具位于 [src/signal_analysis.rs](./src/signal_analysis.rs)。
- 工作负载目录位于 [src/workload_catalog.rs](./src/workload_catalog.rs)。
- 架构说明见 [docs/ARCHITECTURE.md](./docs/ARCHITECTURE.md)。
- 代码审计说明见 [docs/CODE_AUDIT.md](./docs/CODE_AUDIT.md)。
- 推荐表草稿见 [docs/MIDTERM_SUMMARY.md](./docs/MIDTERM_SUMMARY.md)。

## 自动化测试

仓库中已经准备：

1. 单元测试：覆盖队列、状态机、参数校验、控制命令和驱动主流程。
2. CI 工作流：位于 `.github/workflows/rust.yml`，会自动执行 `fmt`、`clippy` 和 `test`。

## 后续扩展方向

1. 接入真实 MMIO 和 DMA 地址映射。
2. 补齐 RX 录音缓冲区投递。
3. 增加 Jack 热插拔和事件队列处理。
4. 与 ArceOS/rCore 的实际设备初始化流程集成。

## 项目维护

```bash
git add .
git commit -m "feat: refine virtio sound driver deliverables"
git push origin main
```
