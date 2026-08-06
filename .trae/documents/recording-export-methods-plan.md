# 录音导出方法扩展实施计划

## Summary
- 目标：在现有 NAPI-RS 录音模块中新增 9 个导出能力：`pause_recording`、`resume_recording`、`is_recording`、`get_record_duration`、4 类状态监听、1 类进度监听。
- 命名：仅导出 `snake_case`（按需求，不增加 camelCase 别名）。
- 语义：暂停后继续写入同一 WAV 文件；监听器支持多监听累加；录音时长单位为毫秒；进度在“每次录音数据落盘时”上报。
- 约束：保持现有 `start_recording/stop_recording` 逻辑主干与队列模型，不引入破坏性重构。

## Current State Analysis
- Rust 核心入口在 `src/lib.rs`：已具备 `do_initialize`、`start_recording`、`stop_recording`。
- 全局状态在 `src/global.rs`：当前仅有 `IS_RECORDING`，尚无“暂停态、累计时长、文件大小、事件监听器池、进度载荷结构”。
- 写盘逻辑在 `start_record_impl()` 中：以循环 `write_sample` 写 WAV，按 `FLUSH_INTERVAL_SECS` 周期 `flush`。
- JS/TS 导出由 NAPI 自动产物体现（`index.js`、`index.d.ts`、`perception-audio-recorder.wasi-browser.js`），当前仅包含初始化与开始/停止能力。

## Proposed Changes

### 1) `src/global.rs`
- 新增全局状态：
  - `IS_PAUSED: AtomicBool`：标记暂停态。
  - `RECORD_DURATION_MS: AtomicU64`：当前会话累计已录制时长（不含暂停期）。
  - `RECORDED_DATA_BYTES: AtomicU64`：累计已写入音频数据字节数（不含 WAV 头）。
  - `CURRENT_OUTPUT_FILE: OnceLock<Mutex<Option<String>>>`（或等价结构）：保存当前输出文件路径，供获取文件总时长/大小时校验上下文。
- 新增 NAPI 对象结构：
  - `RecordingProgressInfo`：`total_duration: u64`、`buffer: Buffer`、`total_size: u64`。
- 新增监听器容器（多监听累加）：
  - `START_RECORDING_LISTENERS`、`STOP_RECORDING_LISTENERS`、`PAUSE_RECORDING_LISTENERS`、`RESUME_RECORDING_LISTENERS`、`PROGRESS_LISTENERS`。
  - 容器使用 `Mutex<Vec<ThreadsafeFunction<...>>>`，支持并发注册与广播。
- 新增通用广播辅助函数：
  - 统一“遍历监听器并非阻塞触发”，失败时记录日志但不中断主流程。

### 2) `src/lib.rs`
- 新增导出方法（`#[napi]`）：
  - `pause_recording() -> napi::Result<()>`
  - `resume_recording() -> napi::Result<()>`
  - `is_recording() -> bool`
  - `get_record_duration() -> u64`
  - `listen_start_recording_audio(listener: ThreadsafeFunction<...>) -> napi::Result<()>`
  - `listen_stop_recording_audio(listener: ThreadsafeFunction<...>) -> napi::Result<()>`
  - `listen_pause_recording_audio(listener: ThreadsafeFunction<...>) -> napi::Result<()>`
  - `listen_resume_recording_audio(listener: ThreadsafeFunction<...>) -> napi::Result<()>`
  - `listen_recording_progress(listener: ThreadsafeFunction<RecordingProgressInfo>) -> napi::Result<()>`
- 对 `start_recording()` 增补：
  - 启动前重置会话级统计：`IS_PAUSED=false`、`RECORD_DURATION_MS=0`、`RECORDED_DATA_BYTES=0`。
  - 创建输出文件后保存文件路径到全局上下文。
  - 启动成功后触发“开始录音”监听回调。
- 对 `stop_recording()` 增补：
  - 保持现有停止语义，状态切换后在录制线程收尾处统一触发“停止录音”监听。
- 对 `start_record_impl()` 核心循环增强：
  - 当 `IS_PAUSED=true`：持续消费队列并丢弃待写样本（避免队列积压），但不写文件、不累计时长与大小。
  - 当可写入时：
    - 继续原有混音与 `write_sample`。
    - 以“本次 flush 周期写入的 PCM 字节块”作为 `buffer` 上报内容（符合“每次落盘上报”）。
    - 累加 `RECORDED_DATA_BYTES`，并按“本周期写入帧数 -> 毫秒”更新 `RECORD_DURATION_MS`。
    - 执行 `flush` 后广播 `listen_recording_progress`，载荷字段：
      - `total_duration`: `RECORD_DURATION_MS`
      - `buffer`: 本周期音频字节（`ArrayBuffer` 可直接从 Node `Buffer` 获取底层）
      - `total_size`: `44 + RECORDED_DATA_BYTES`（WAV 头 + 数据体）
  - 录制结束收尾：
    - 最终 `flush` 后再发一次进度（若有剩余块），随后触发“停止录音”事件。
    - 清理当前文件路径与暂停态，保证下次会话干净启动。
- 新增暂停/恢复状态方法细则：
  - `pause_recording`：仅在 `is_recording=true` 且未暂停时允许；否则返回语义化错误。
  - `resume_recording`：仅在 `is_recording=true` 且已暂停时允许；否则返回语义化错误。
  - 两者成功后分别广播 pause/resume 事件。

### 3) 导出与类型文件（自动产物）
- 通过项目现有 NAPI 构建链刷新导出声明（实现阶段执行）：
  - `index.js`：新增 9 个 `module.exports` 绑定。
  - `index.d.ts`：新增函数签名与 `RecordingProgressInfo` 类型声明。
  - `perception-audio-recorder.wasi-browser.js`：同步新增导出桥接。
- 仅保留 `snake_case` 新增接口名称，不添加 camelCase 同义导出。

### 4) 测试与示例
- `test.js`（或新增最小示例脚本）扩展手工验证流程：
  - 初始化 -> 开始 -> 暂停 -> 恢复 -> 停止。
  - 验证四类状态回调触发顺序。
  - 验证进度回调字段完整性（时长递增、`buffer.byteLength > 0`、`total_size` 递增）。
- 若引入自动化测试，优先补充轻量接口级测试，避免依赖真实音频设备的脆弱测试。

## Assumptions & Decisions
- `is_recording` 定义为“录制会话是否处于进行中（含暂停态）”；暂停不等于停止。
- `get_record_duration` 返回当前会话“已落盘音频累计时长（毫秒）”，不包含暂停阶段。
- `listen_recording_progress` 的 `buffer` 为“本次落盘块”的 PCM16LE 字节，不回传整段历史音频，避免内存膨胀。
- 监听器采用“累加注册、无移除 API”策略；重复注册由调用方自行管理。
- 进度触发点定义为 `flush` 时刻（周期 flush + 结束前 flush）。

## Verification Steps
1. 运行构建：`npm run build`，确认 NAPI 生成物包含新增导出且无编译错误。
2. 运行类型检查：确认 `index.d.ts` 中新增方法与进度对象字段完整。
3. 运行手工脚本：
   - 开始录制后 `is_recording=true`。
   - 暂停后不再增长时长；恢复后继续增长。
   - 停止后 `is_recording=false`，并触发停止事件。
4. 校验进度回调：
   - `total_duration` 单调不减。
   - `total_size` 单调不减。
   - `buffer` 可在 JS 端按 `ArrayBuffer`/`Uint8Array` 正常读取。
5. 兼容回归：确认原有 `do_initialize/start_recording/stop_recording` 行为与错误语义未回退。
