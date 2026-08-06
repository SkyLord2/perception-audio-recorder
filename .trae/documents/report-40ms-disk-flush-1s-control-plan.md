# 上报 40ms 与磁盘刷新 1s 解耦控制计划

## Summary

- 目标：保持 `listen_recording_progress` 以 40ms 分片上报，同时将“磁盘强制安全刷新（writer.flush）”改为每 1 秒一次。
- 关键要求：在 `global.rs` 中新增“上报间隔”和“磁盘刷新间隔”控制项，避免硬编码散落在 `lib.rs`。
- 成功标准：
  - 上报分片仍按公式 `sampleRate × 16bit × channels ÷ 8 × interval_sec`，默认 40ms。
  - `writer.flush()` 不再随每次上报执行，而是按 1 秒节拍触发。
  - stop 收尾时仍执行最终 `flush`，确保数据安全落盘。

## Current State Analysis

- `src/lib.rs` 当前已有 `const PROGRESS_CHUNK_MS: u64 = 40`，并基于该值计算 `target_chunk_bytes`。
- 当前 `emit_progress_chunk(...)` 内部每次都会执行 `writer.flush()`，导致 40ms 上报频率直接等同于磁盘强制刷新频率。
- 循环末尾仍有 `writer.flush()`，用于 stop 收尾。
- `src/global.rs` 当前没有“上报间隔/磁盘刷新间隔”的独立常量配置入口。

## Proposed Changes

### 1) 在 `global.rs` 增加统一间隔控制常量

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\global.rs`
- What：
  - 新增常量（命名可按当前风格统一）：
    - `PROGRESS_REPORT_INTERVAL_MS: u64 = 40`
    - `DISK_FLUSH_INTERVAL_MS: u64 = 1000`
- Why：
  - 将时间策略集中配置，便于后续调优与文档同步。
- How：
  - 常量放在录音链路参数区，与 `PACKET_QUEUE_MAX`、`CHANNEL_QUEUE_MAX_SECONDS` 同层级。

### 2) `lib.rs` 改为引用全局间隔常量

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 删除/替换局部 `PROGRESS_CHUNK_MS`，改用 `global::PROGRESS_REPORT_INTERVAL_MS`。
  - `target_chunk_bytes` 计算改为基于全局上报间隔常量。
- Why：
  - 满足“在 global.rs 控制上报间隔”的要求。

### 3) 解耦“上报”与“磁盘强制刷新”

- 文件：`e:\Code\HaierGit\perception-audio-recorder\src\lib.rs`
- What：
  - 从 `emit_progress_chunk(...)` 中移除 `writer.flush()`。
  - 新增磁盘 flush 节拍控制（例如 `last_disk_flush_at: Instant`）：
    - 录制循环中若 `elapsed >= DISK_FLUSH_INTERVAL_MS` 执行一次 `writer.flush()`；
    - flush 成功后重置计时锚点。
  - 末尾收尾保持 `writer.flush()`（stop 场景强制落盘）。
- Why：
  - 让上报保持 40ms 高频，而磁盘安全刷新保持 1s，避免 I/O 过频影响性能。
- How：
  - 上报逻辑继续按 `while flush_pcm_chunk.len() >= target_chunk_bytes` 分片发送，不受磁盘 flush 节拍影响。
  - 若循环空转也可触发 1s flush（当已有写入但未到 stop）。

### 4) 日志与文档同步

- 文件：`e:\Code\HaierGit\perception-audio-recorder\README.md`
- What：
  - 补充“上报间隔与磁盘刷新间隔已解耦”的说明：
    - 上报默认 40ms；
    - 强制磁盘刷新默认 1s；
    - stop 时会立即做最终刷新。
- Why：
  - 避免调用方误解“40ms 上报等于 40ms 落盘”。

## Assumptions & Decisions

- 决策 1：默认上报间隔保持 40ms，不改当前分片公式与语义。
- 决策 2：默认磁盘强制刷新间隔为 1000ms。
- 决策 3：`recordDevice=0/1` 的空侧上报语义保持不变。
- 决策 4：stop 收尾必须立即 flush，不受 1s 间隔限制。

## Verification steps

1. 编译与诊断
   - `cargo check` 通过；
   - `lib.rs`、`global.rs`、`README.md` 无新增诊断错误。
2. 上报频率与字节验证
   - `sampleRate=16000, mono` 时 `progress.buffer.byteLength` 常态为 `1280`，上报周期约 40ms。
3. 磁盘刷新节拍验证
   - 日志中可观测到 flush 节拍约 1s（非每个 progress 都 flush）。
4. 收尾验证
   - stop 后立即完成最后一次 flush，文件可正常回放且无尾段丢失。
