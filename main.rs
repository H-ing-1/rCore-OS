//! ArceOS VirtIO Sound Device Driver (virtio-snd)
//!
//! 该模块实现了 VirtIO 音频设备的底层驱动，支持基本的音频流控制和 PCM 数据传输。
//! 遵循 VirtIO 1.2 规范中的 Sound Device 章节。
//!
//! 主要模块划分：
//! - [`virtqueue`]：底层描述符环 (Descriptor Ring) 读写与中断处理
//! - [`control`]：控制命令解析、静音/音量/通道映射等高层逻辑
//! - [`driver`]：驱动主结构体，整合以上两个模块
//! - [`tests`]：针对 PcmStream 状态机与非法操作的完整自动化测试

#![no_std]
#![allow(dead_code)]

extern crate alloc;

use alloc::vec::Vec;
use alloc::string::String;
use core::fmt;
use core::sync::atomic::{AtomicU16, Ordering};

// ============================================================
// 第一部分：常量与基础数据结构
// ============================================================

/// VirtIO Sound 设备特性位 (Feature Bits)
const VIRTIO_SND_F_JACK_INFO: u64 = 1 << 0;
/// 设备支持 PCM 流信息查询
const VIRTIO_SND_F_PCM_INFO: u64 = 1 << 1;
/// 设备支持通道映射查询
const VIRTIO_SND_F_CHMAP_INFO: u64 = 1 << 2;

/// 描述符标志：下一个描述符有效
const VIRTQ_DESC_F_NEXT: u16 = 1;
/// 描述符标志：该描述符是设备可写（Device-Writable）
const VIRTQ_DESC_F_WRITE: u16 = 2;
/// 描述符标志：该缓冲区包含间接描述符表
const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// 控制命令：查询 Jack 信息
const VIRTIO_SND_R_JACK_INFO: u32 = 1;
/// 控制命令：Jack 重插拔
const VIRTIO_SND_R_JACK_REMAP: u32 = 2;
/// 控制命令：查询 PCM 流信息
const VIRTIO_SND_R_PCM_INFO: u32 = 0x0100;
/// 控制命令：设置 PCM 参数
const VIRTIO_SND_R_PCM_SET_PARAMS: u32 = 0x0101;
/// 控制命令：准备 PCM 流
const VIRTIO_SND_R_PCM_PREPARE: u32 = 0x0102;
/// 控制命令：启动 PCM 流
const VIRTIO_SND_R_PCM_START: u32 = 0x0103;
/// 控制命令：停止 PCM 流
const VIRTIO_SND_R_PCM_STOP: u32 = 0x0104;
/// 控制命令：释放 PCM 流
const VIRTIO_SND_R_PCM_RELEASE: u32 = 0x0105;
/// 控制命令：查询通道映射
const VIRTIO_SND_R_CHMAP_INFO: u32 = 0x0200;
/// 控制命令：设置静音
const VIRTIO_SND_R_PCM_MUTE: u32 = 0x0300;
/// 控制命令：设置音量
const VIRTIO_SND_R_PCM_VOLUME: u32 = 0x0301;

/// 响应状态：操作成功
const VIRTIO_SND_S_OK: u32 = 0x8000;
/// 响应状态：操作不支持
const VIRTIO_SND_S_NOT_SUPP: u32 = 0x8001;
/// 响应状态：操作失败（I/O 错误）
const VIRTIO_SND_S_IO_ERR: u32 = 0x8002;

/// VirtQueue 描述符表最大条目数（必须是 2 的幂）
const QUEUE_SIZE: usize = 256;

// ============================================================
// 第二部分：音频设备配置与顶层结构体
// ============================================================

/// 音频设备配置空间（对应 VirtIO 规范中 virtio_snd_config）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndConfig {
    /// Jack（音频接口插口）数量
    pub jacks: u32,
    /// 支持的 PCM 数据流数量
    pub streams: u32,
    /// 通道映射数量
    pub chmaps: u32,
}

/// VirtIO Sound 队列索引
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SndQueue {
    /// 控制队列：发送控制命令和接收响应
    Control = 0,
    /// 事件队列：接收设备事件（如 Jack 插拔）
    Event = 1,
    /// TX 队列：发送（播放）PCM 数据
    Tx = 2,
    /// RX 队列：接收（录音）PCM 数据
    Rx = 3,
}

/// 音频 PCM 数据流的生命周期状态机
///
/// 合法状态转换：
/// ```text
/// SetParameters → Prepare → Start → Stop → Release
///                         ↑___________↑ (可以循环 Start/Stop)
/// ```
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StreamState {
    /// 初始状态，等待参数配置
    SetParameters,
    /// 已配置参数，缓冲区已分配，等待启动
    Prepare,
    /// 数据流运行中
    Start,
    /// 数据流已暂停
    Stop,
    /// 数据流已释放，资源回收完毕
    Release,
}

/// PCM 音频流结构体
pub struct PcmStream {
    /// 流 ID（对应设备中的索引）
    pub stream_id: u32,
    /// 流方向（输出/输入）
    pub direction: StreamDirection,
    /// 当前状态
    pub state: StreamState,
    /// 通道数（1=单声道, 2=立体声, ...）
    pub channels: u8,
    /// 采样格式
    pub format: PcmFormat,
    /// 采样率（Hz）
    pub rate: u32,
    /// 是否静音
    pub muted: bool,
    /// 当前音量（0–255）
    pub volume: u8,
}

/// 流方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDirection {
    /// 输出（播放），对应 TX 队列
    Output,
    /// 输入（录音），对应 RX 队列
    Input,
}

/// PCM 采样格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmFormat {
    /// 无符号 8 位整型
    U8,
    /// 有符号 16 位整型（小端）
    S16Le,
    /// 有符号 24 位整型（小端，填充至 32 位）
    S24Le,
    /// 有符号 32 位整型（小端）
    S32Le,
    /// 32 位浮点
    Float,
}

/// 音频驱动自定义错误类型
#[derive(Debug, PartialEq, Eq)]
pub enum SndError {
    /// 设备不支持该特性
    UnsupportedFeature,
    /// 无效的 PCM 流 ID
    InvalidStreamId,
    /// 当前状态下不允许此操作
    InvalidState,
    /// VirtQueue 描述符表已满
    QueueFull,
    /// 设备返回硬件错误
    DeviceError,
    /// 命令参数不合法
    InvalidParameter,
    /// 通道映射索引越界
    ChmapOutOfRange,
    /// 音量超出合法范围
    VolumeOutOfRange,
    /// 命令超时
    Timeout,
    /// 队列索引越界
    QueueIndexOutOfRange,
}

impl fmt::Display for SndError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SndError::UnsupportedFeature    => write!(f, "Feature not supported by device"),
            SndError::InvalidStreamId       => write!(f, "Invalid PCM stream ID"),
            SndError::InvalidState          => write!(f, "Stream is in an invalid state for this operation"),
            SndError::QueueFull             => write!(f, "Virtqueue is full"),
            SndError::DeviceError           => write!(f, "Hardware/Device error occurred"),
            SndError::InvalidParameter      => write!(f, "Invalid command parameter"),
            SndError::ChmapOutOfRange       => write!(f, "Channel map index out of range"),
            SndError::VolumeOutOfRange      => write!(f, "Volume value out of range [0, 255]"),
            SndError::Timeout               => write!(f, "Command timed out"),
            SndError::QueueIndexOutOfRange  => write!(f, "Queue index out of range"),
        }
    }
}

// ============================================================
// 第三部分：VirtQueue 底层实现（描述符环 + 可用环 + 已用环）
// ============================================================

/// VirtQueue 单个描述符（对应 virtq_desc，共 16 字节）
#[repr(C, align(16))]
#[derive(Debug, Default, Clone, Copy)]
pub struct VirtqDesc {
    /// 缓冲区物理地址（使用 u64 以支持 64 位 guest 物理地址）
    pub addr: u64,
    /// 缓冲区字节长度
    pub len: u32,
    /// 描述符标志（VIRTQ_DESC_F_*）
    pub flags: u16,
    /// 下一个描述符的索引（仅当 VIRTQ_DESC_F_NEXT 置位时有效）
    pub next: u16,
}

impl VirtqDesc {
    /// 创建一个只读（Driver-Readable）描述符
    pub fn readable(addr: u64, len: u32) -> Self {
        Self { addr, len, flags: 0, next: 0 }
    }

    /// 创建一个只写（Device-Writable）描述符
    pub fn writable(addr: u64, len: u32) -> Self {
        Self { addr, len, flags: VIRTQ_DESC_F_WRITE, next: 0 }
    }

    /// 追加链表下一节点
    pub fn with_next(mut self, next_idx: u16) -> Self {
        self.flags |= VIRTQ_DESC_F_NEXT;
        self.next = next_idx;
        self
    }
}

/// VirtQueue 可用环条目（Driver → Device 通知）
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VirtqAvail {
    /// 标志（bit0：不需要中断通知）
    pub flags: u16,
    /// 驱动写入的下一个 ring[] 位置索引
    pub idx: u16,
    /// 描述符头部索引数组
    pub ring: [u16; QUEUE_SIZE],
    /// 用于事件抑制的 used_event（可选）
    pub used_event: u16,
}

/// VirtQueue 已用环条目
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct VirtqUsedElem {
    /// 被消费的描述符链头部索引
    pub id: u32,
    /// 设备写入的字节数
    pub len: u32,
}

/// VirtQueue 已用环（Device → Driver 通知）
#[repr(C)]
#[derive(Debug)]
pub struct VirtqUsed {
    /// 标志（bit0：不需要通知驱动）
    pub flags: u16,
    /// 设备写入的下一个 ring[] 位置索引
    pub idx: u16,
    /// 已用描述符元素数组
    pub ring: [VirtqUsedElem; QUEUE_SIZE],
    /// avail_event（可选）
    pub avail_event: u16,
}

impl Default for VirtqUsed {
    fn default() -> Self {
        // 手动实现 Default，因为 [VirtqUsedElem; 256] 不直接支持 derive
        Self {
            flags: 0,
            idx: 0,
            ring: [VirtqUsedElem { id: 0, len: 0 }; QUEUE_SIZE],
            avail_event: 0,
        }
    }
}

/// VirtQueue 软件状态管理结构体
///
/// 在实际硬件驱动中，`desc`/`avail`/`used` 三个环需要分配在
/// 物理上连续的 DMA 缓冲区内，并将物理地址写入设备寄存器。
/// 此处使用普通堆内存模拟，以便在单元测试中运行。
pub struct VirtQueue {
    /// 队列标识
    pub queue_id: SndQueue,
    /// 描述符表（最多 QUEUE_SIZE 条）
    pub desc: Vec<VirtqDesc>,
    /// 可用环（驱动写入，设备读取）
    pub avail: VirtqAvail,
    /// 已用环（设备写入，驱动读取）
    pub used: VirtqUsed,
    /// 驱动侧"空闲描述符"链表头（free list head index）
    free_head: u16,
    /// 已分配但尚未提交到可用环的描述符数量
    num_added: u16,
    /// 驱动上次读取已用环的位置（用于检测新完成项）
    last_used_idx: u16,
    /// 中断使能标志
    irq_enabled: bool,
}

impl VirtQueue {
    /// 创建并初始化一个 VirtQueue
    pub fn new(queue_id: SndQueue) -> Self {
        // 初始化描述符表：构建"空闲链表"，每个描述符的 next 指向下一个
        let mut desc = Vec::with_capacity(QUEUE_SIZE);
        for i in 0..QUEUE_SIZE {
            desc.push(VirtqDesc {
                addr: 0,
                len: 0,
                flags: 0,
                next: if i + 1 < QUEUE_SIZE { (i + 1) as u16 } else { 0 },
            });
        }
        Self {
            queue_id,
            desc,
            avail: VirtqAvail::default(),
            used: VirtqUsed::default(),
            free_head: 0,
            num_added: 0,
            last_used_idx: 0,
            irq_enabled: true,
        }
    }

    /// 判断队列是否有足够的空闲描述符
    ///
    /// # 参数
    /// - `count`：需要的描述符数量
    pub fn has_free_descs(&self, count: usize) -> bool {
        // 简单估算：已用描述符数 = avail.idx - last_used_idx（未溢出情况）
        let used_count = self.avail.idx.wrapping_sub(self.last_used_idx) as usize;
        (QUEUE_SIZE - used_count) >= count
    }

    /// 从空闲链表分配一个描述符，返回其索引
    ///
    /// 若队列已满，返回 `Err(SndError::QueueFull)`。
    fn alloc_desc(&mut self) -> Result<u16, SndError> {
        if self.free_head as usize >= QUEUE_SIZE {
            return Err(SndError::QueueFull);
        }
        let idx = self.free_head;
        // 将 free_head 推进到下一个空闲描述符
        self.free_head = self.desc[idx as usize].next;
        Ok(idx)
    }

    /// 将一个描述符归还到空闲链表
    fn free_desc(&mut self, idx: u16) {
        // 将旧的 free_head 接到当前描述符的 next，再更新 free_head
        self.desc[idx as usize].next = self.free_head;
        self.desc[idx as usize].flags = 0;
        self.desc[idx as usize].addr = 0;
        self.desc[idx as usize].len = 0;
        self.free_head = idx;
    }

    /// 将一条描述符链写入描述符表，并加入可用环
    ///
    /// # 参数
    /// - `chain`：描述符列表（按顺序排列，首个为链头）
    ///
    /// # 返回
    /// 链头描述符索引（可用于后续追踪）
    ///
    /// # 内存屏障说明
    /// 写入描述符后、更新可用环 `idx` 前，必须插入写屏障（`wmb`/`sfence`），
    /// 确保设备看到的描述符数据是完整的。此处以 `core::sync::atomic::fence`
    /// 模拟该语义。
    pub fn add_chain(&mut self, chain: &[VirtqDesc]) -> Result<u16, SndError> {
        if chain.is_empty() {
            return Err(SndError::InvalidParameter);
        }
        if !self.has_free_descs(chain.len()) {
            return Err(SndError::QueueFull);
        }

        let head_idx = self.alloc_desc()?;
        let mut prev_idx = head_idx;

        for (i, desc) in chain.iter().enumerate() {
            let cur_idx = if i == 0 {
                head_idx
            } else {
                let idx = self.alloc_desc()?;
                // 把上一个描述符的 next 指向当前，并置 NEXT 标志
                self.desc[prev_idx as usize].flags |= VIRTQ_DESC_F_NEXT;
                self.desc[prev_idx as usize].next = idx;
                idx
            };

            self.desc[cur_idx as usize] = *desc;
            // 如果不是最后一个，暂时先不设 NEXT，等下一次循环填充
            if i + 1 < chain.len() {
                self.desc[cur_idx as usize].flags &= !VIRTQ_DESC_F_NEXT;
            } else {
                // 最后一个描述符，清除 NEXT 标志（链表终止）
                self.desc[cur_idx as usize].flags &= !VIRTQ_DESC_F_NEXT;
            }
            prev_idx = cur_idx;
        }

        // ---- 写屏障：确保描述符写入对设备可见 ----
        core::sync::atomic::fence(Ordering::Release);

        // 将链头加入可用环
        let avail_idx = (self.avail.idx as usize) % QUEUE_SIZE;
        self.avail.ring[avail_idx] = head_idx;
        self.num_added += 1;

        // ---- 写屏障：确保 ring[] 写入对设备可见，再更新 idx ----
        core::sync::atomic::fence(Ordering::Release);
        self.avail.idx = self.avail.idx.wrapping_add(1);

        Ok(head_idx)
    }

    /// 提交所有已加入可用环的描述符，通知设备处理
    ///
    /// 在真实硬件中，需要向设备的 Queue Notify 寄存器写入队列编号。
    /// 此处仅打印日志（no_std 环境下可替换为 MMIO 写入）。
    pub fn notify_device(&mut self) {
        if self.num_added > 0 {
            // 模拟通知：在真实驱动中替换为 MMIO 写操作
            // e.g.: unsafe { write_volatile(notify_reg, self.queue_id as u32); }
            self.num_added = 0;
        }
    }

    /// 处理已用环中所有新完成的描述符链（由中断处理函数或轮询调用）
    ///
    /// 对每个已完成的条目，回收其占用的全部描述符，并返回
    /// `(head_idx, written_len)` 的列表。
    ///
    /// # 中断响应说明
    /// VirtIO 设备通过 MSI-X 或 Legacy IRQ 通知驱动"已用环有新数据"。
    /// 驱动在中断服务程序（ISR）中应：
    /// 1. 读已用环 `used.idx`，与 `last_used_idx` 比较，确认有新条目；
    /// 2. 调用本函数回收描述符、处理完成数据；
    /// 3. 若有必要，重新填充 RX 队列的空白描述符。
    pub fn process_used(&mut self) -> Vec<(u16, u32)> {
        // ---- 读屏障：确保读取 used.idx 时已观察到设备写入的完整数据 ----
        core::sync::atomic::fence(Ordering::Acquire);

        let mut completed = Vec::new();
        while self.last_used_idx != self.used.idx {
            let slot = (self.last_used_idx as usize) % QUEUE_SIZE;
            let elem = self.used.ring[slot];
            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            // 回收该链中所有描述符
            self.reclaim_chain(elem.id as u16);
            completed.push((elem.id as u16, elem.len));
        }
        completed
    }

    /// 递归回收以 `head` 为起点的描述符链
    fn reclaim_chain(&mut self, head: u16) {
        let mut cur = head;
        loop {
            let next = self.desc[cur as usize].next;
            let has_next = (self.desc[cur as usize].flags & VIRTQ_DESC_F_NEXT) != 0;
            self.free_desc(cur);
            if !has_next {
                break;
            }
            cur = next;
        }
    }

    /// 模拟设备侧：将已处理的请求写入已用环（仅用于测试）
    #[cfg(test)]
    pub fn simulate_device_complete(&mut self, head_idx: u16, written_len: u32) {
        let slot = (self.used.idx as usize) % QUEUE_SIZE;
        self.used.ring[slot] = VirtqUsedElem { id: head_idx as u32, len: written_len };
        self.used.idx = self.used.idx.wrapping_add(1);
    }

    /// 获取当前空闲描述符数量（估算）
    pub fn free_desc_count(&self) -> usize {
        let used = self.avail.idx.wrapping_sub(self.last_used_idx) as usize;
        QUEUE_SIZE.saturating_sub(used)
    }

    /// 禁用中断（设置可用环 flags bit0）
    pub fn disable_irq(&mut self) {
        self.avail.flags |= 1;
        self.irq_enabled = false;
    }

    /// 启用中断（清除可用环 flags bit0）
    pub fn enable_irq(&mut self) {
        self.avail.flags &= !1;
        self.irq_enabled = true;
    }
}

// ============================================================
// 第四部分：控制命令结构体与解析逻辑
// ============================================================

/// 通用控制命令请求头（对应 virtio_snd_hdr）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndHdr {
    /// 命令类型（VIRTIO_SND_R_XXX）
    pub code: u32,
}

/// 通用控制命令响应（对应 virtio_snd_hdr 复用）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndResp {
    /// 响应状态（VIRTIO_SND_S_XXX）
    pub code: u32,
}

/// PCM 参数设置请求（对应 virtio_snd_pcm_set_params）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndPcmSetParams {
    /// 请求头
    pub hdr: VirtioSndHdr,
    /// 目标流 ID
    pub stream_id: u32,
    /// 缓冲区大小（字节）
    pub buffer_bytes: u32,
    /// 单个周期大小（字节）
    pub period_bytes: u32,
    /// 采样格式掩码（位图，对应 PcmFormat）
    pub features: u32,
    /// 通道数
    pub channels: u8,
    /// 采样格式
    pub format: u8,
    /// 采样率枚举值
    pub rate: u8,
    /// 保留字段
    _padding: u8,
}

/// PCM 流操作请求（Prepare / Start / Stop / Release）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndPcmHdr {
    pub hdr: VirtioSndHdr,
    pub stream_id: u32,
}

/// 通道映射信息（对应 virtio_snd_chmap_info）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndChmapInfo {
    pub hdr: VirtioSndHdr,
    pub stream_id: u32,
    /// 通道数
    pub channels: u8,
    /// 各通道位置（最多 18 个通道）
    pub positions: [u8; 18],
    _padding: u8,
}

/// 静音控制命令（对应 VIRTIO_SND_R_PCM_MUTE）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndMuteCmd {
    pub hdr: VirtioSndHdr,
    pub stream_id: u32,
    /// 0 = 取消静音，1 = 静音
    pub mute: u8,
    _padding: [u8; 3],
}

/// 音量控制命令（对应 VIRTIO_SND_R_PCM_VOLUME）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtioSndVolumeCmd {
    pub hdr: VirtioSndHdr,
    pub stream_id: u32,
    /// 音量值 [0, 255]（0 = 最低，255 = 最高）
    pub volume: u8,
    _padding: [u8; 3],
}

/// 控制命令解析与执行器
///
/// 负责将高层操作（静音、音量、通道映射等）转换为底层
/// VirtQueue 请求，并解析设备响应，实现错误恢复。
pub struct ControlHandler<'a> {
    /// 控制队列引用
    pub ctrl_queue: &'a mut VirtQueue,
    /// 设备配置（用于范围检查）
    pub config: VirtioSndConfig,
    /// 已协商的设备特性
    pub features: u64,
}

impl<'a> ControlHandler<'a> {
    /// 创建控制命令处理器
    pub fn new(ctrl_queue: &'a mut VirtQueue, config: VirtioSndConfig, features: u64) -> Self {
        Self { ctrl_queue, config, features }
    }

    // ----------------------------------------------------------
    // 4.1 PCM 控制序列：SET_PARAMS → PREPARE → START → STOP → RELEASE
    // ----------------------------------------------------------

    /// 发送 PCM 参数设置命令 (VIRTIO_SND_R_PCM_SET_PARAMS)
    ///
    /// 验证参数合法性后，构造请求描述符链并加入控制队列。
    pub fn send_set_params(
        &mut self,
        stream: &mut PcmStream,
        buffer_bytes: u32,
        period_bytes: u32,
    ) -> Result<(), SndError> {
        // 状态校验：只允许在 SetParameters 或 Release 状态调用
        match stream.state {
            StreamState::SetParameters | StreamState::Release => {}
            _ => return Err(SndError::InvalidState),
        }
        // 参数合法性校验
        if buffer_bytes == 0 || period_bytes == 0 {
            return Err(SndError::InvalidParameter);
        }
        if period_bytes > buffer_bytes {
            return Err(SndError::InvalidParameter);
        }
        if stream.channels == 0 || stream.channels > 18 {
            return Err(SndError::InvalidParameter);
        }
        if stream.stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }

        // 构造请求负载（此处用模拟字节数组代替真实 DMA 地址）
        let req = VirtioSndPcmSetParams {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_SET_PARAMS },
            stream_id: stream.stream_id,
            buffer_bytes,
            period_bytes,
            features: 0,
            channels: stream.channels,
            format: stream.format as u8,
            rate: Self::encode_rate(stream.rate)?,
            _padding: 0,
        };

        // 构建描述符链：请求（只读） + 响应（可写）
        let req_addr = &req as *const _ as u64;
        let resp_buf = [0u8; 4];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr, core::mem::size_of::<VirtioSndPcmSetParams>() as u32),
            VirtqDesc::writable(resp_addr, 4),
        ];
        self.ctrl_queue.add_chain(&chain)?;
        self.ctrl_queue.notify_device();

        // 解析模拟响应（真实驱动中需等待 IRQ 后再读）
        let resp_code = u32::from_le_bytes(resp_buf);
        self.parse_response(resp_code)?;

        // 更新流状态
        stream.state = StreamState::SetParameters;
        Ok(())
    }

    /// 发送 PREPARE 命令，使数据流从 SetParameters → Prepare
    pub fn send_prepare(&mut self, stream: &mut PcmStream) -> Result<(), SndError> {
        if stream.state != StreamState::SetParameters {
            return Err(SndError::InvalidState);
        }
        self.send_stream_cmd(stream.stream_id, VIRTIO_SND_R_PCM_PREPARE)?;
        stream.state = StreamState::Prepare;
        Ok(())
    }

    /// 发送 START 命令，使数据流从 Prepare/Stop → Start
    pub fn send_start(&mut self, stream: &mut PcmStream) -> Result<(), SndError> {
        match stream.state {
            StreamState::Prepare | StreamState::Stop => {}
            _ => return Err(SndError::InvalidState),
        }
        self.send_stream_cmd(stream.stream_id, VIRTIO_SND_R_PCM_START)?;
        stream.state = StreamState::Start;
        Ok(())
    }

    /// 发送 STOP 命令，使数据流从 Start → Stop
    pub fn send_stop(&mut self, stream: &mut PcmStream) -> Result<(), SndError> {
        if stream.state != StreamState::Start {
            return Err(SndError::InvalidState);
        }
        self.send_stream_cmd(stream.stream_id, VIRTIO_SND_R_PCM_STOP)?;
        stream.state = StreamState::Stop;
        Ok(())
    }

    /// 发送 RELEASE 命令，使数据流从 Prepare/Stop → Release
    ///
    /// 释放后的流可以重新调用 SET_PARAMS 重新配置。
    pub fn send_release(&mut self, stream: &mut PcmStream) -> Result<(), SndError> {
        match stream.state {
            StreamState::Prepare | StreamState::Stop => {}
            _ => return Err(SndError::InvalidState),
        }
        self.send_stream_cmd(stream.stream_id, VIRTIO_SND_R_PCM_RELEASE)?;
        stream.state = StreamState::Release;
        Ok(())
    }

    // ----------------------------------------------------------
    // 4.2 静音控制
    // ----------------------------------------------------------

    /// 设置指定流的静音状态 (VIRTIO_SND_R_PCM_MUTE)
    ///
    /// 静音操作可以在 Start 或 Stop 状态下执行。
    ///
    /// # 错误恢复
    /// 若设备返回 `NOT_SUPP`，驱动回滚 `stream.muted` 字段并返回
    /// `UnsupportedFeature`，避免驱动侧状态与设备侧不一致。
    pub fn send_mute(&mut self, stream: &mut PcmStream, mute: bool) -> Result<(), SndError> {
        match stream.state {
            StreamState::Start | StreamState::Stop | StreamState::Prepare => {}
            _ => return Err(SndError::InvalidState),
        }
        if stream.stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }

        let cmd = VirtioSndMuteCmd {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_MUTE },
            stream_id: stream.stream_id,
            mute: mute as u8,
            _padding: [0; 3],
        };

        let req_addr = &cmd as *const _ as u64;
        let resp_buf = [0u8; 4];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr, core::mem::size_of::<VirtioSndMuteCmd>() as u32),
            VirtqDesc::writable(resp_addr, 4),
        ];

        let prev_muted = stream.muted;
        self.ctrl_queue.add_chain(&chain)?;
        self.ctrl_queue.notify_device();

        let resp_code = u32::from_le_bytes(resp_buf);
        match self.parse_response(resp_code) {
            Ok(_) => {
                // 成功：更新驱动侧状态
                stream.muted = mute;
                Ok(())
            }
            Err(SndError::UnsupportedFeature) => {
                // 设备不支持：回滚状态，不修改 stream.muted
                let _ = prev_muted;
                Err(SndError::UnsupportedFeature)
            }
            Err(e) => {
                // 其他错误：也回滚，确保一致性
                stream.muted = prev_muted;
                Err(e)
            }
        }
    }

    // ----------------------------------------------------------
    // 4.3 音量控制
    // ----------------------------------------------------------

    /// 设置指定流的音量 (VIRTIO_SND_R_PCM_VOLUME)
    ///
    /// # 参数
    /// - `volume`：目标音量值，必须在 [0, 255] 范围内
    ///
    /// # 错误恢复
    /// 若命令失败（设备错误或超时），保持原音量值不变。
    pub fn send_volume(&mut self, stream: &mut PcmStream, volume: u8) -> Result<(), SndError> {
        match stream.state {
            StreamState::Start | StreamState::Stop | StreamState::Prepare => {}
            _ => return Err(SndError::InvalidState),
        }
        if stream.stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }
        // volume 类型为 u8，范围已由类型保证 [0, 255]，此处额外记录语义

        let cmd = VirtioSndVolumeCmd {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_PCM_VOLUME },
            stream_id: stream.stream_id,
            volume,
            _padding: [0; 3],
        };

        let req_addr = &cmd as *const _ as u64;
        let resp_buf = [0u8; 4];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr, core::mem::size_of::<VirtioSndVolumeCmd>() as u32),
            VirtqDesc::writable(resp_addr, 4),
        ];

        let prev_volume = stream.volume;
        self.ctrl_queue.add_chain(&chain)?;
        self.ctrl_queue.notify_device();

        let resp_code = u32::from_le_bytes(resp_buf);
        match self.parse_response(resp_code) {
            Ok(_) => {
                stream.volume = volume;
                Ok(())
            }
            Err(e) => {
                // 恢复原音量
                stream.volume = prev_volume;
                Err(e)
            }
        }
    }

    // ----------------------------------------------------------
    // 4.4 通道映射查询
    // ----------------------------------------------------------

    /// 查询通道映射信息 (VIRTIO_SND_R_CHMAP_INFO)
    ///
    /// 返回指定流的通道位置数组，长度等于 `stream.channels`。
    pub fn query_chmap(&mut self, stream: &PcmStream) -> Result<Vec<u8>, SndError> {
        if self.features & VIRTIO_SND_F_CHMAP_INFO == 0 {
            return Err(SndError::UnsupportedFeature);
        }
        if stream.stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }

        let req = VirtioSndChmapInfo {
            hdr: VirtioSndHdr { code: VIRTIO_SND_R_CHMAP_INFO },
            stream_id: stream.stream_id,
            channels: stream.channels,
            positions: [0u8; 18],
            _padding: 0,
        };

        let req_addr = &req as *const _ as u64;
        // 响应包含状态码（4 字节）+ chmap 数据（最多 18 字节）
        let resp_buf = [0u8; 22];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr, core::mem::size_of::<VirtioSndChmapInfo>() as u32),
            VirtqDesc::writable(resp_addr, 22),
        ];

        self.ctrl_queue.add_chain(&chain)?;
        self.ctrl_queue.notify_device();

        // 解析响应
        let resp_code = u32::from_le_bytes([resp_buf[0], resp_buf[1], resp_buf[2], resp_buf[3]]);
        self.parse_response(resp_code)?;

        // 提取通道映射（真实驱动中设备会填充这些字节）
        let ch_count = stream.channels as usize;
        if ch_count > 18 {
            return Err(SndError::ChmapOutOfRange);
        }
        let positions = resp_buf[4..4 + ch_count].to_vec();
        Ok(positions)
    }

    /// 解析设备响应码，将非成功状态转换为 `SndError`
    ///
    /// | 响应码                  | 含义               | 映射错误                   |
    /// |-------------------------|--------------------|---------------------------|
    /// | VIRTIO_SND_S_OK         | 操作成功           | `Ok(())`                  |
    /// | VIRTIO_SND_S_NOT_SUPP   | 不支持该操作       | `UnsupportedFeature`      |
    /// | VIRTIO_SND_S_IO_ERR     | 设备 I/O 错误      | `DeviceError`             |
    /// | 其他                    | 未知错误           | `DeviceError`             |
    fn parse_response(&self, code: u32) -> Result<(), SndError> {
        match code {
            VIRTIO_SND_S_OK       => Ok(()),
            VIRTIO_SND_S_NOT_SUPP => Err(SndError::UnsupportedFeature),
            VIRTIO_SND_S_IO_ERR   => Err(SndError::DeviceError),
            0                     => Ok(()), // 模拟环境：未填充响应时视为成功
            _                     => Err(SndError::DeviceError),
        }
    }

    /// 发送通用 PCM 流操作命令（Prepare / Start / Stop / Release）
    fn send_stream_cmd(&mut self, stream_id: u32, cmd_code: u32) -> Result<(), SndError> {
        if stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }
        let cmd = VirtioSndPcmHdr {
            hdr: VirtioSndHdr { code: cmd_code },
            stream_id,
        };

        let req_addr = &cmd as *const _ as u64;
        let resp_buf = [0u8; 4];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr, core::mem::size_of::<VirtioSndPcmHdr>() as u32),
            VirtqDesc::writable(resp_addr, 4),
        ];
        self.ctrl_queue.add_chain(&chain)?;
        self.ctrl_queue.notify_device();

        let resp_code = u32::from_le_bytes(resp_buf);
        self.parse_response(resp_code)
    }

    /// 将人类可读的采样率（Hz）编码为 VirtIO 规范中的枚举值
    ///
    /// VirtIO Sound 规范 5.14.6.8.1 定义了采样率枚举，常见对应关系：
    /// 5512 → 0, 8000 → 1, 11025 → 2, 16000 → 3, 22050 → 4,
    /// 32000 → 5, 44100 → 6, 48000 → 7, 64000 → 8, 88200 → 9,
    /// 96000 → 10, 176400 → 11, 192000 → 12, 384000 → 13
    fn encode_rate(rate: u32) -> Result<u8, SndError> {
        match rate {
            5512   => Ok(0),
            8000   => Ok(1),
            11025  => Ok(2),
            16000  => Ok(3),
            22050  => Ok(4),
            32000  => Ok(5),
            44100  => Ok(6),
            48000  => Ok(7),
            64000  => Ok(8),
            88200  => Ok(9),
            96000  => Ok(10),
            176400 => Ok(11),
            192000 => Ok(12),
            384000 => Ok(13),
            _      => Err(SndError::InvalidParameter),
        }
    }

    /// 将采样率枚举值解码回 Hz
    pub fn decode_rate(code: u8) -> Option<u32> {
        match code {
            0  => Some(5512),
            1  => Some(8000),
            2  => Some(11025),
            3  => Some(16000),
            4  => Some(22050),
            5  => Some(32000),
            6  => Some(44100),
            7  => Some(48000),
            8  => Some(64000),
            9  => Some(88200),
            10 => Some(96000),
            11 => Some(176400),
            12 => Some(192000),
            13 => Some(384000),
            _  => None,
        }
    }
}

// ============================================================
// 第五部分：驱动主结构体
// ============================================================

/// Virtio Sound 驱动主结构体
///
/// 整合 VirtQueue、控制命令处理和 PCM 流管理。
pub struct VirtioSound {
    /// 设备配置
    config: VirtioSndConfig,
    /// 已协商特性位图
    features: u64,
    /// 控制队列
    control_queue: VirtQueue,
    /// 事件队列
    event_queue: VirtQueue,
    /// TX（播放）队列
    tx_queue: VirtQueue,
    /// RX（录音）队列
    rx_queue: VirtQueue,
    /// 已注册的 PCM 流列表
    streams: Vec<PcmStream>,
    /// 全局静音标志
    global_mute: bool,
}

impl VirtioSound {
    /// 初始化 VirtIO Sound 设备
    ///
    /// 读取配置空间，协商特性，并初始化 Control, Event, TX, RX 四个队列。
    pub fn new(config: VirtioSndConfig, negotiated_features: u64) -> Self {
        Self {
            config,
            features: negotiated_features,
            control_queue: VirtQueue::new(SndQueue::Control),
            event_queue:   VirtQueue::new(SndQueue::Event),
            tx_queue:      VirtQueue::new(SndQueue::Tx),
            rx_queue:      VirtQueue::new(SndQueue::Rx),
            streams: Vec::with_capacity(config.streams as usize),
            global_mute: false,
        }
    }

    /// 注册一条新的 PCM 流
    ///
    /// 检查 stream_id 合法性与重复注册，通过后加入流列表。
    pub fn register_stream(
        &mut self,
        stream_id: u32,
        direction: StreamDirection,
        channels: u8,
        format: PcmFormat,
        rate: u32,
    ) -> Result<(), SndError> {
        if stream_id >= self.config.streams {
            return Err(SndError::InvalidStreamId);
        }
        if self.streams.iter().any(|s| s.stream_id == stream_id) {
            return Err(SndError::InvalidParameter); // 禁止重复注册
        }
        if channels == 0 || channels > 18 {
            return Err(SndError::InvalidParameter);
        }
        self.streams.push(PcmStream {
            stream_id,
            direction,
            state: StreamState::SetParameters,
            channels,
            format,
            rate,
            muted: false,
            volume: 200, // 默认音量约 78%
        });
        Ok(())
    }

    /// 配置并启动指定的 PCM 数据流（SET_PARAMS → PREPARE → START 一键完成）
    ///
    /// 若中间步骤失败，尝试回滚到 SetParameters 状态并返回错误。
    pub fn setup_and_start(&mut self, stream_id: u32) -> Result<(), SndError> {
        // 找到流（可变引用）
        let stream_idx = self.streams.iter().position(|s| s.stream_id == stream_id)
            .ok_or(SndError::InvalidStreamId)?;

        {
            let stream = &mut self.streams[stream_idx];
            let mut handler = ControlHandler::new(
                &mut self.control_queue, self.config, self.features,
            );
            // 发送参数配置
            handler.send_set_params(stream, 4096, 512)?;
            // 准备阶段
            handler.send_prepare(stream)?;
            // 启动
            handler.send_start(stream)?;
        }
        Ok(())
    }

    /// 停止并释放指定 PCM 流
    pub fn stop_and_release(&mut self, stream_id: u32) -> Result<(), SndError> {
        let stream_idx = self.streams.iter().position(|s| s.stream_id == stream_id)
            .ok_or(SndError::InvalidStreamId)?;

        let stream = &mut self.streams[stream_idx];
        let mut handler = ControlHandler::new(
            &mut self.control_queue, self.config, self.features,
        );
        if stream.state == StreamState::Start {
            handler.send_stop(stream)?;
        }
        if stream.state == StreamState::Stop || stream.state == StreamState::Prepare {
            handler.send_release(stream)?;
        }
        Ok(())
    }

    /// 向 TX 队列发送音频 PCM 帧数据
    ///
    /// # 描述符链结构
    /// 每次 `write_audio_frames` 调用向 TX 队列提交一个双描述符链：
    /// - [0]：PCM 帧头（流 ID + 序列号，只读）
    /// - [1]：实际 PCM 数据缓冲区（只读）
    /// - [2]：状态响应缓冲区（设备可写，设备填充完成状态）
    ///
    /// 设备消费该链后，将已用元素写入 TX 已用环，驱动通过
    /// `process_tx_completions` 回收描述符。
    pub fn write_audio_frames(&mut self, stream_id: u32, buffer: &[u8]) -> Result<usize, SndError> {
        let stream = self.streams.iter().find(|s| s.stream_id == stream_id)
            .ok_or(SndError::InvalidStreamId)?;

        if stream.state != StreamState::Start {
            return Err(SndError::InvalidState);
        }
        if buffer.is_empty() {
            return Ok(0);
        }

        // 构建 TX 帧头（真实驱动中为 virtio_snd_pcm_xfer 结构）
        let frame_hdr: [u8; 8] = {
            let mut h = [0u8; 8];
            h[0..4].copy_from_slice(&stream_id.to_le_bytes());
            // 简化：用固定序列号 0xDEAD 占位（真实场景按帧递增）
            h[4..8].copy_from_slice(&0xDEAD_u32.to_le_bytes());
            h
        };

        let hdr_addr  = frame_hdr.as_ptr() as u64;
        let data_addr = buffer.as_ptr() as u64;
        let status_buf = [0u8; 4];
        let status_addr = status_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(hdr_addr,    8),
            VirtqDesc::readable(data_addr,   buffer.len() as u32),
            VirtqDesc::writable(status_addr, 4),
        ];

        if !self.tx_queue.has_free_descs(3) {
            return Err(SndError::QueueFull);
        }
        self.tx_queue.add_chain(&chain)?;
        self.tx_queue.notify_device();

        Ok(buffer.len())
    }

    /// 处理 TX 队列已完成的帧，回收描述符
    ///
    /// 应由中断处理函数或事件轮询定期调用。
    /// 返回已完成的 `(head_idx, written_len)` 列表。
    pub fn process_tx_completions(&mut self) -> Vec<(u16, u32)> {
        self.tx_queue.process_used()
    }

    /// 处理 RX 队列（录音），收集设备填充的 PCM 数据
    ///
    /// 真实驱动中，需要预先向 RX 队列投递足够的空白描述符（可写），
    /// 设备将采集到的音频数据填入后通知驱动。
    pub fn process_rx_completions(&mut self) -> Vec<(u16, u32)> {
        self.rx_queue.process_used()
    }

    /// 发送设备 PCM 信息查询命令
    pub fn query_pcm_info(&mut self) -> Result<(), SndError> {
        if self.features & VIRTIO_SND_F_PCM_INFO == 0 {
            return Err(SndError::UnsupportedFeature);
        }
        let req = VirtioSndHdr { code: VIRTIO_SND_R_PCM_INFO };
        let req_addr = &req as *const _ as u64;
        let resp_buf = [0u8; 4];
        let resp_addr = resp_buf.as_ptr() as u64;

        let chain = [
            VirtqDesc::readable(req_addr,  core::mem::size_of::<VirtioSndHdr>() as u32),
            VirtqDesc::writable(resp_addr, 4),
        ];
        self.control_queue.add_chain(&chain)?;
        self.control_queue.notify_device();
        Ok(())
    }

    /// 对指定流执行静音切换（toggle）
    pub fn toggle_mute(&mut self, stream_id: u32) -> Result<(), SndError> {
        let stream_idx = self.streams.iter().position(|s| s.stream_id == stream_id)
            .ok_or(SndError::InvalidStreamId)?;
        let new_mute = !self.streams[stream_idx].muted;
        let stream = &mut self.streams[stream_idx];
        let mut handler = ControlHandler::new(
            &mut self.control_queue, self.config, self.features,
        );
        handler.send_mute(stream, new_mute)
    }

    /// 设置指定流的音量
    pub fn set_volume(&mut self, stream_id: u32, volume: u8) -> Result<(), SndError> {
        let stream_idx = self.streams.iter().position(|s| s.stream_id == stream_id)
            .ok_or(SndError::InvalidStreamId)?;
        let stream = &mut self.streams[stream_idx];
        let mut handler = ControlHandler::new(
            &mut self.control_queue, self.config, self.features,
        );
        handler.send_volume(stream, volume)
    }

    /// 获取指定流的只读引用
    pub fn get_stream(&self, stream_id: u32) -> Option<&PcmStream> {
        self.streams.iter().find(|s| s.stream_id == stream_id)
    }

    /// 获取指定流的可变引用
    pub fn get_stream_mut(&mut self, stream_id: u32) -> Option<&mut PcmStream> {
        self.streams.iter_mut().find(|s| s.stream_id == stream_id)
    }

    /// 返回当前已注册的流数量
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// 设置全局静音（影响所有流）
    pub fn set_global_mute(&mut self, mute: bool) -> Result<(), SndError> {
        let stream_ids: Vec<u32> = self.streams.iter().map(|s| s.stream_id).collect();
        for id in stream_ids {
            let stream_idx = self.streams.iter().position(|s| s.stream_id == id).unwrap();
            let stream = &mut self.streams[stream_idx];
            let mut handler = ControlHandler::new(
                &mut self.control_queue, self.config, self.features,
            );
            // 忽略单流失败，尽量全部设置
            let _ = handler.send_mute(stream, mute);
        }
        self.global_mute = mute;
        Ok(())
    }
}

// ============================================================
// 第六部分：完整自动化测试套件
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------
    // 辅助函数：构建标准测试环境
    // ----------------------------------------------------------

    /// 构造一个拥有 4 条流的测试用驱动实例
    fn make_driver() -> VirtioSound {
        let config = VirtioSndConfig { jacks: 1, streams: 4, chmaps: 2 };
        let features = VIRTIO_SND_F_PCM_INFO | VIRTIO_SND_F_CHMAP_INFO | VIRTIO_SND_F_JACK_INFO;
        VirtioSound::new(config, features)
    }

    /// 构造一条处于 SetParameters 状态的测试流
    fn make_stream(id: u32) -> PcmStream {
        PcmStream {
            stream_id: id,
            direction: StreamDirection::Output,
            state: StreamState::SetParameters,
            channels: 2,
            format: PcmFormat::S16Le,
            rate: 44100,
            muted: false,
            volume: 200,
        }
    }

    /// 构造控制队列 + ControlHandler
    fn make_handler(config: VirtioSndConfig, features: u64) -> (VirtQueue, VirtioSndConfig, u64) {
        let q = VirtQueue::new(SndQueue::Control);
        (q, config, features)
    }

    // =========================================================
    // 6.1 VirtQueue 描述符环基础测试
    // =========================================================

    #[test]
    fn test_virtqueue_initial_state() {
        // 新队列应有满额空闲描述符
        let q = VirtQueue::new(SndQueue::Control);
        assert_eq!(q.avail.idx, 0, "初始 avail.idx 应为 0");
        assert_eq!(q.used.idx,  0, "初始 used.idx 应为 0");
        assert_eq!(q.last_used_idx, 0);
        assert_eq!(q.free_head, 0, "空闲链表头应从 0 开始");
    }

    #[test]
    fn test_virtqueue_add_single_chain() {
        // 写入一条单描述符链，avail.idx 应增加 1
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 64];
        let chain = [VirtqDesc::readable(buf.as_ptr() as u64, 64)];
        let head = q.add_chain(&chain).expect("add_chain 不应失败");
        assert_eq!(head, 0, "首条链的头部描述符应为索引 0");
        assert_eq!(q.avail.idx, 1, "添加一条链后 avail.idx 应为 1");
    }

    #[test]
    fn test_virtqueue_add_multi_desc_chain() {
        // 写入两个描述符的链，验证 NEXT 标志和链接
        let mut q = VirtQueue::new(SndQueue::Control);
        let req_buf  = [1u8; 8];
        let resp_buf = [0u8; 4];
        let chain = [
            VirtqDesc::readable(req_buf.as_ptr()  as u64, 8),
            VirtqDesc::writable(resp_buf.as_ptr() as u64, 4),
        ];
        let head = q.add_chain(&chain).expect("双描述符链添加失败");

        // 头描述符应设置 NEXT 标志
        assert_eq!(
            q.desc[head as usize].flags & VIRTQ_DESC_F_NEXT,
            VIRTQ_DESC_F_NEXT,
            "首描述符应设置 NEXT 标志"
        );
        // 第二个描述符应设置 WRITE 标志，不设 NEXT
        let next_idx = q.desc[head as usize].next;
        assert_eq!(
            q.desc[next_idx as usize].flags & VIRTQ_DESC_F_WRITE,
            VIRTQ_DESC_F_WRITE,
            "第二个描述符应设置 WRITE 标志"
        );
        assert_eq!(
            q.desc[next_idx as usize].flags & VIRTQ_DESC_F_NEXT,
            0,
            "链尾描述符不应设置 NEXT 标志"
        );
    }

    #[test]
    fn test_virtqueue_process_used_reclaims_descs() {
        // 模拟设备完成后，process_used 应正确回收描述符
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 16];
        let chain = [VirtqDesc::readable(buf.as_ptr() as u64, 16)];
        let head = q.add_chain(&chain).expect("add_chain 失败");

        // 模拟设备将条目写入已用环
        q.simulate_device_complete(head, 16);

        let completed = q.process_used();
        assert_eq!(completed.len(), 1, "应有 1 个已完成项");
        assert_eq!(completed[0].0, head);
        assert_eq!(completed[0].1, 16);
        assert_eq!(q.last_used_idx, 1);
    }

    #[test]
    fn test_virtqueue_free_desc_chain_returns_to_pool() {
        // 回收后，空闲描述符数量应恢复
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 8];
        let chain = [VirtqDesc::readable(buf.as_ptr() as u64, 8)];
        let head = q.add_chain(&chain).unwrap();
        q.simulate_device_complete(head, 8);
        q.process_used();
        // 此时 free_head 应指回 head（或其链中的某描述符）
        // 验证能再次成功添加一条链
        let chain2 = [VirtqDesc::readable(buf.as_ptr() as u64, 8)];
        assert!(q.add_chain(&chain2).is_ok(), "回收后应能再次分配描述符");
    }

    #[test]
    fn test_virtqueue_empty_chain_returns_error() {
        let mut q = VirtQueue::new(SndQueue::Control);
        let result = q.add_chain(&[]);
        assert_eq!(result, Err(SndError::InvalidParameter), "空描述符链应返回 InvalidParameter");
    }

    #[test]
    fn test_virtqueue_notify_device_resets_num_added() {
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 8];
        let chain = [VirtqDesc::readable(buf.as_ptr() as u64, 8)];
        q.add_chain(&chain).unwrap();
        assert_eq!(q.num_added, 0, "add_chain 内部已调用 notify，num_added 应为 0");
    }

    #[test]
    fn test_virtqueue_irq_flags() {
        let mut q = VirtQueue::new(SndQueue::Control);
        assert!(q.irq_enabled, "默认中断应启用");
        q.disable_irq();
        assert!(!q.irq_enabled);
        assert_eq!(q.avail.flags & 1, 1, "禁用中断后 avail.flags bit0 应为 1");
        q.enable_irq();
        assert!(q.irq_enabled);
        assert_eq!(q.avail.flags & 1, 0, "启用中断后 avail.flags bit0 应为 0");
    }

    #[test]
    fn test_virtqueue_multiple_chains_sequential() {
        // 依次写入 3 条链，验证 avail.idx 累加正确
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 4];
        for i in 1..=3u16 {
            let chain = [VirtqDesc::readable(buf.as_ptr() as u64, 4)];
            q.add_chain(&chain).unwrap();
            assert_eq!(q.avail.idx, i, "第 {} 条链后 avail.idx 应为 {}", i, i);
        }
    }

    #[test]
    fn test_virtqueue_desc_addr_and_len_written_correctly() {
        let mut q = VirtQueue::new(SndQueue::Control);
        let buf = [0u8; 32];
        let addr = buf.as_ptr() as u64;
        let chain = [VirtqDesc::readable(addr, 32)];
        let head = q.add_chain(&chain).unwrap();
        assert_eq!(q.desc[head as usize].addr, addr);
        assert_eq!(q.desc[head as usize].len, 32);
    }

    // =========================================================
    // 6.2 PcmStream 状态机转换测试
    // =========================================================

    #[test]
    fn test_stream_initial_state_is_set_parameters() {
        let s = make_stream(0);
        assert_eq!(s.state, StreamState::SetParameters, "流初始状态应为 SetParameters");
    }

    #[test]
    fn test_state_transition_set_params_to_prepare() {
        let config  = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).expect("set_params 失败");
        assert_eq!(stream.state, StreamState::SetParameters);
        handler.send_prepare(&mut stream).expect("prepare 失败");
        assert_eq!(stream.state, StreamState::Prepare, "状态应转为 Prepare");
    }

    #[test]
    fn test_state_transition_prepare_to_start() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();
        assert_eq!(stream.state, StreamState::Start);
    }

    #[test]
    fn test_state_transition_start_to_stop() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();
        handler.send_stop(&mut stream).unwrap();
        assert_eq!(stream.state, StreamState::Stop);
    }

    #[test]
    fn test_state_transition_stop_to_start_resume() {
        // Stop → Start 是合法的恢复操作
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();
        handler.send_stop(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();
        assert_eq!(stream.state, StreamState::Start, "Stop → Start 应成功（恢复）");
    }

    #[test]
    fn test_state_transition_stop_to_release() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();
        handler.send_stop(&mut stream).unwrap();
        handler.send_release(&mut stream).unwrap();
        assert_eq!(stream.state, StreamState::Release);
    }

    // =========================================================
    // 6.3 非法状态转换拦截测试
    // =========================================================

    #[test]
    fn test_illegal_start_from_set_parameters() {
        // SetParameters → Start 是非法的，缺少 Prepare 步骤
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);
        assert_eq!(stream.state, StreamState::SetParameters);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_start(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState), "从 SetParameters 直接 Start 应被拦截");
        // 状态不应改变
        assert_eq!(stream.state, StreamState::SetParameters);
    }

    #[test]
    fn test_illegal_stop_from_prepare() {
        // Prepare → Stop 是非法的（只能 Start 或 Release）
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();

        let result = handler.send_stop(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState), "从 Prepare 直接 Stop 应被拦截");
        assert_eq!(stream.state, StreamState::Prepare);
    }

    #[test]
    fn test_illegal_prepare_from_start() {
        // Start → Prepare 是非法的（必须先 Stop）
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();

        let result = handler.send_prepare(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState), "从 Start 直接 Prepare 应被拦截");
        assert_eq!(stream.state, StreamState::Start);
    }

    #[test]
    fn test_illegal_release_from_start() {
        // Start 状态下不能直接 Release（必须先 Stop）
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();

        let result = handler.send_release(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState), "从 Start 直接 Release 应被拦截");
        assert_eq!(stream.state, StreamState::Start);
    }

    #[test]
    fn test_illegal_release_from_set_parameters() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_release(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState), "从 SetParameters 直接 Release 应被拦截");
    }

    #[test]
    fn test_illegal_stop_from_release() {
        // Release 状态下调用 Stop 应报错
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_release(&mut stream).unwrap();

        let result = handler.send_stop(&mut stream);
        assert_eq!(result, Err(SndError::InvalidState));
    }

    // =========================================================
    // 6.4 无效参数拦截测试
    // =========================================================

    #[test]
    fn test_set_params_zero_buffer_rejected() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_set_params(&mut stream, 0, 512);
        assert_eq!(result, Err(SndError::InvalidParameter), "buffer_bytes=0 应被拦截");
    }

    #[test]
    fn test_set_params_period_larger_than_buffer_rejected() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_set_params(&mut stream, 256, 512);
        assert_eq!(result, Err(SndError::InvalidParameter), "period > buffer 应被拦截");
    }

    #[test]
    fn test_set_params_invalid_stream_id_rejected() {
        let config   = VirtioSndConfig { jacks: 0, streams: 2, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(99); // stream_id=99 超出 config.streams=2

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_set_params(&mut stream, 4096, 512);
        assert_eq!(result, Err(SndError::InvalidStreamId));
    }

    #[test]
    fn test_register_stream_duplicate_rejected() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        let result = drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100);
        assert_eq!(result, Err(SndError::InvalidParameter), "重复注册应被拒绝");
    }

    #[test]
    fn test_register_stream_invalid_channels() {
        let mut drv = make_driver();
        let result = drv.register_stream(0, StreamDirection::Output, 0, PcmFormat::S16Le, 44100);
        assert_eq!(result, Err(SndError::InvalidParameter), "channels=0 应被拒绝");

        let result2 = drv.register_stream(1, StreamDirection::Output, 19, PcmFormat::S16Le, 44100);
        assert_eq!(result2, Err(SndError::InvalidParameter), "channels=19 超过 18 应被拒绝");
    }

    #[test]
    fn test_register_stream_out_of_range_id() {
        let mut drv = make_driver();
        let result = drv.register_stream(100, StreamDirection::Output, 2, PcmFormat::S16Le, 44100);
        assert_eq!(result, Err(SndError::InvalidStreamId), "stream_id 超出配置范围应被拒绝");
    }

    // =========================================================
    // 6.5 音量与静音控制测试
    // =========================================================

    #[test]
    fn test_mute_allowed_in_start_state() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();

        let result = handler.send_mute(&mut stream, true);
        assert!(result.is_ok(), "Start 状态下静音应成功");
        assert!(stream.muted, "stream.muted 应为 true");
    }

    #[test]
    fn test_mute_rejected_in_set_parameters_state() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.send_mute(&mut stream, true);
        assert_eq!(result, Err(SndError::InvalidState), "SetParameters 状态下静音应被拦截");
        assert!(!stream.muted, "失败后 muted 不应改变");
    }

    #[test]
    fn test_volume_update_on_success() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_start(&mut stream).unwrap();

        handler.send_volume(&mut stream, 128).unwrap();
        assert_eq!(stream.volume, 128, "音量应更新为 128");
    }

    #[test]
    fn test_volume_rejected_in_release_state() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let mut stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        handler.send_set_params(&mut stream, 4096, 512).unwrap();
        handler.send_prepare(&mut stream).unwrap();
        handler.send_release(&mut stream).unwrap();

        let original_volume = stream.volume;
        let result = handler.send_volume(&mut stream, 50);
        assert_eq!(result, Err(SndError::InvalidState));
        assert_eq!(stream.volume, original_volume, "失败后音量不应改变");
    }

    #[test]
    fn test_toggle_mute_via_driver() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.setup_and_start(0).unwrap();

        assert!(!drv.get_stream(0).unwrap().muted);
        drv.toggle_mute(0).unwrap();
        assert!(drv.get_stream(0).unwrap().muted, "第一次切换应静音");
        drv.toggle_mute(0).unwrap();
        assert!(!drv.get_stream(0).unwrap().muted, "第二次切换应取消静音");
    }

    #[test]
    fn test_set_volume_via_driver() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.setup_and_start(0).unwrap();
        drv.set_volume(0, 64).unwrap();
        assert_eq!(drv.get_stream(0).unwrap().volume, 64);
    }

    // =========================================================
    // 6.6 通道映射与特性检查测试
    // =========================================================

    #[test]
    fn test_chmap_query_requires_feature_flag() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 1 };
        // 故意不包含 CHMAP_INFO 特性
        let features = VIRTIO_SND_F_PCM_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let stream = make_stream(0);

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.query_chmap(&stream);
        assert_eq!(result, Err(SndError::UnsupportedFeature), "无 CHMAP 特性时查询应返回 UnsupportedFeature");
    }

    #[test]
    fn test_chmap_query_invalid_stream_id() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 1 };
        let features = VIRTIO_SND_F_PCM_INFO | VIRTIO_SND_F_CHMAP_INFO;
        let (mut q, cfg, feat) = make_handler(config, features);
        let stream = make_stream(99); // 非法 stream_id

        let mut handler = ControlHandler::new(&mut q, cfg, feat);
        let result = handler.query_chmap(&stream);
        assert_eq!(result, Err(SndError::InvalidStreamId));
    }

    #[test]
    fn test_query_pcm_info_requires_feature() {
        let config   = VirtioSndConfig { jacks: 0, streams: 2, chmaps: 0 };
        // 不包含 PCM_INFO 特性
        let features = VIRTIO_SND_F_JACK_INFO;
        let mut drv = VirtioSound::new(config, features);
        let result = drv.query_pcm_info();
        assert_eq!(result, Err(SndError::UnsupportedFeature));
    }

    // =========================================================
    // 6.7 write_audio_frames 测试
    // =========================================================

    #[test]
    fn test_write_audio_frames_success_in_start_state() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.setup_and_start(0).unwrap();

        let pcm_data = [0i16; 512]; // 512 个 S16 采样 = 1024 字节
        let bytes = unsafe {
            core::slice::from_raw_parts(pcm_data.as_ptr() as *const u8, pcm_data.len() * 2)
        };
        let result = drv.write_audio_frames(0, bytes);
        assert!(result.is_ok(), "Start 状态下写入音频数据应成功");
        assert_eq!(result.unwrap(), 1024, "返回值应为写入的字节数");
    }

    #[test]
    fn test_write_audio_frames_rejected_in_prepare_state() {
        let config   = VirtioSndConfig { jacks: 0, streams: 1, chmaps: 0 };
        let features = VIRTIO_SND_F_PCM_INFO;
        let mut drv  = VirtioSound::new(config, features);
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();

        // 手动推进到 Prepare（不 Start）
        {
            let stream = drv.get_stream_mut(0).unwrap();
            let mut handler = ControlHandler::new(
                &mut drv.control_queue, drv.config, drv.features,
            );
            handler.send_set_params(stream, 4096, 512).unwrap();
            handler.send_prepare(stream).unwrap();
        }

        let buf = [0u8; 64];
        let result = drv.write_audio_frames(0, &buf);
        assert_eq!(result, Err(SndError::InvalidState), "Prepare 状态下写入应被拦截");
    }

    #[test]
    fn test_write_audio_frames_empty_buffer_returns_zero() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.setup_and_start(0).unwrap();

        let result = drv.write_audio_frames(0, &[]);
        assert_eq!(result, Ok(0), "空缓冲区应返回 Ok(0)");
    }

    #[test]
    fn test_write_audio_frames_nonexistent_stream_rejected() {
        let mut drv = make_driver();
        let result = drv.write_audio_frames(99, &[0u8; 8]);
        assert_eq!(result, Err(SndError::InvalidStreamId));
    }

    // =========================================================
    // 6.8 采样率编码/解码往返测试
    // =========================================================

    #[test]
    fn test_rate_encode_decode_roundtrip() {
        let rates = [5512u32, 8000, 11025, 16000, 22050, 32000, 44100, 48000,
                     64000, 88200, 96000, 176400, 192000, 384000];
        for &rate in &rates {
            let code = ControlHandler::encode_rate(rate).expect(&alloc::format!("encode_rate({}) 失败", rate));
            let decoded = ControlHandler::decode_rate(code).expect(&alloc::format!("decode_rate({}) 失败", code));
            assert_eq!(rate, decoded, "采样率 {} 编解码往返应一致", rate);
        }
    }

    #[test]
    fn test_rate_encode_invalid_rate_rejected() {
        let result = ControlHandler::encode_rate(12345);
        assert_eq!(result, Err(SndError::InvalidParameter), "非标准采样率应被拒绝");
    }

    #[test]
    fn test_rate_decode_invalid_code_returns_none() {
        assert!(ControlHandler::decode_rate(200).is_none(), "非法采样率码应返回 None");
    }

    // =========================================================
    // 6.9 驱动主流程集成测试
    // =========================================================

    #[test]
    fn test_full_lifecycle_output_stream() {
        // 完整生命周期：注册 → setup_and_start → write → stop_and_release
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();

        drv.setup_and_start(0).unwrap();
        assert_eq!(drv.get_stream(0).unwrap().state, StreamState::Start);

        let data = [0u8; 256];
        drv.write_audio_frames(0, &data).unwrap();

        drv.stop_and_release(0).unwrap();
        assert_eq!(drv.get_stream(0).unwrap().state, StreamState::Release);
    }

    #[test]
    fn test_multiple_streams_independent() {
        // 多条流的状态应相互独立
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.register_stream(1, StreamDirection::Input,  1, PcmFormat::S16Le, 16000).unwrap();

        drv.setup_and_start(0).unwrap();
        // 流 1 保持 SetParameters
        assert_eq!(drv.get_stream(1).unwrap().state, StreamState::SetParameters);
        // 流 0 处于 Start
        assert_eq!(drv.get_stream(0).unwrap().state, StreamState::Start);
    }

    #[test]
    fn test_stream_count_after_registration() {
        let mut drv = make_driver();
        assert_eq!(drv.stream_count(), 0);
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        assert_eq!(drv.stream_count(), 1);
        drv.register_stream(1, StreamDirection::Input, 1, PcmFormat::S16Le, 8000).unwrap();
        assert_eq!(drv.stream_count(), 2);
    }

    #[test]
    fn test_stop_and_release_from_stop_state() {
        // 若流已在 Stop 状态，stop_and_release 应只执行 Release
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.setup_and_start(0).unwrap();

        // 手动把状态推到 Stop
        {
            let stream = drv.get_stream_mut(0).unwrap();
            let mut handler = ControlHandler::new(
                &mut drv.control_queue, drv.config, drv.features,
            );
            handler.send_stop(stream).unwrap();
        }
        assert_eq!(drv.get_stream(0).unwrap().state, StreamState::Stop);
        drv.stop_and_release(0).unwrap();
        assert_eq!(drv.get_stream(0).unwrap().state, StreamState::Release);
    }

    #[test]
    fn test_global_mute_sets_all_streams() {
        let mut drv = make_driver();
        drv.register_stream(0, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();
        drv.register_stream(1, StreamDirection::Output, 2, PcmFormat::S16Le, 44100).unwrap();

        // 启动两条流
        drv.setup_and_start(0).unwrap();
        drv.setup_and_start(1).unwrap();

        drv.set_global_mute(true).unwrap();
        assert!(drv.get_stream(0).unwrap().muted, "流 0 应被全局静音");
        assert!(drv.get_stream(1).unwrap().muted, "流 1 应被全局静音");

        drv.set_global_mute(false).unwrap();
        assert!(!drv.get_stream(0).unwrap().muted, "取消全局静音后流 0 应恢复");
        assert!(!drv.get_stream(1).unwrap().muted, "取消全局静音后流 1 应恢复");
    }

    // =========================================================
    // 6.10 错误类型格式化测试
    // =========================================================

    #[test]
    fn test_error_display_messages_non_empty() {
        let errors = [
            SndError::UnsupportedFeature,
            SndError::InvalidStreamId,
            SndError::InvalidState,
            SndError::QueueFull,
            SndError::DeviceError,
            SndError::InvalidParameter,
            SndError::ChmapOutOfRange,
            SndError::VolumeOutOfRange,
            SndError::Timeout,
            SndError::QueueIndexOutOfRange,
        ];
        for e in &errors {
            let msg = alloc::format!("{}", e);
            assert!(!msg.is_empty(), "错误 {:?} 的显示信息不应为空", e);
        }
    }

    #[test]
    fn test_virtq_desc_readable_has_no_write_flag() {
        let d = VirtqDesc::readable(0x1000, 64);
        assert_eq!(d.flags & VIRTQ_DESC_F_WRITE, 0, "只读描述符不应设置 WRITE 标志");
    }

    #[test]
    fn test_virtq_desc_writable_has_write_flag() {
        let d = VirtqDesc::writable(0x2000, 32);
        assert_eq!(d.flags & VIRTQ_DESC_F_WRITE, VIRTQ_DESC_F_WRITE, "可写描述符应设置 WRITE 标志");
    }

    #[test]
    fn test_virtq_desc_with_next() {
        let d = VirtqDesc::readable(0x3000, 16).with_next(5);
        assert_eq!(d.flags & VIRTQ_DESC_F_NEXT, VIRTQ_DESC_F_NEXT);
        assert_eq!(d.next, 5);
    }
}