# 架构设计

## 1. 目标

本项目面向开源社区唐图 `rCore-OS` 赛道，提供一个可审计、可扩展的 `VirtIO Sound` 驱动原型实现。重点不是堆砌接口，而是把驱动核心流程做成能够被阅读、复核、测试和后续迁移到真实内核环境的代码基线。

## 2. 分层

当前源码集中在 [src/lib.rs](../src/lib.rs)，但逻辑已经按职责分为四层：

1. 设备能力与协议常量层
   - `VirtioSndConfig`
   - `SndQueue`
   - `PcmFormat`
   - `StreamState`

2. 队列与描述符管理层
   - `VirtqDesc`
   - `VirtqAvail`
   - `VirtqUsed`
   - `VirtQueue`

3. 控制面命令层
   - `ControlHandler`
   - `VirtioSndPcmSetParams`
   - `VirtioSndMuteCmd`
   - `VirtioSndVolumeCmd`
   - `VirtioSndChmapInfo`

4. 驱动编排层
   - `VirtioSound`
   - 流注册
   - 生命周期管理
   - TX/RX 队列调用
   - 全局静音与音量控制

## 3. 独立功能

项目至少包含四类可独立评分的功能：

1. `VirtQueue` 描述符环管理与完成项回收。
2. PCM 流生命周期控制：`SetParameters -> Prepare -> Start -> Stop -> Release`。
3. 播放链路：向 TX 队列提交 PCM 帧。
4. 控制链路：静音、音量、通道映射查询、PCM 信息查询。

## 4. 数据流

### 4.1 控制命令

`VirtioSound` 通过 `ControlHandler` 构造控制请求，再将请求封装为描述符链写入控制队列。控制命令统一经过响应码解析，确保：

- 设备不支持时返回 `UnsupportedFeature`
- 设备错误时返回 `DeviceError`
- 模拟环境下空响应允许按成功路径推进状态机

### 4.2 播放链路

播放路径采用三段式描述符链：

1. 帧头
2. PCM 数据
3. 状态缓冲区

这样可以清晰审计每一帧的输入边界、设备写入位置和回收时机。

### 4.3 录音链路

当前实现保留了 RX 队列完成处理入口 `process_rx_completions`，便于后续接入预投递缓冲区和真实录音路径。

## 5. 自动测试设计

测试聚焦五个方向：

1. `VirtQueue` 基础行为。
2. 状态机合法流转。
3. 非法状态和非法参数拦截。
4. 音量、静音、通道映射相关功能。
5. 驱动主流程集成行为。

## 6. 后续演进

若继续往真实内核环境演进，建议按以下顺序拆模块：

1. `src/spec.rs`：协议常量和请求结构。
2. `src/virtqueue.rs`：队列与 DMA 相关逻辑。
3. `src/control.rs`：控制命令编排。
4. `src/driver.rs`：驱动主结构与对外 API。
5. `src/tests.rs` 或 `tests/`：独立测试入口。

当前保持单文件主体实现，是为了在竞赛提交阶段优先保障审计连续性和评审阅读成本。
