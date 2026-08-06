# `test.js` 将 `micBuffer` 保存为本地文件实施计划

## Summary

- 目标：在现有测试脚本 `test.js` 中，将 `listenRecordingProgress` 上报的 `progress.micBuffer` 持续写入本地文件，并在 `stopRecording` 后完成文件收尾。
- 范围：仅改动 `e:\Code\HaierGit\perception-audio-recorder\test.js`，不改 Rust 录音核心与导出 API。
- 成功标准：
  - 录制启动时创建麦克风输出文件（建议命名为 `mic_progress_<timestamp>.wav`）。
  - 录制过程中持续追加 `micBuffer` 数据。
  - 停止后正确回填 WAV 头并关闭文件句柄。
  - 控制台日志可看到 mic 文件路径、累计字节、收尾结果。

## Current State Analysis

- 当前 `test.js` 已实现扬声器链路落盘：
  - 使用 `spkFd/spkDataBytes/spkWavPath` 维护写入状态。
  - 在 `listenRecordingProgress` 中写入 `progress.spkBuffer`。
  - 在停止后调用 `writeWavHeader(spkFd, spkDataBytes)` 回填并关闭句柄。
- 当前 `RecordingProgressInfo` 已包含 `micBuffer` 字段，脚本也打印了 `progress.micBuffer.byteLength`，说明数据已经可用。
- 当前脚本仅有一个 WAV 头写入函数 `writeWavHeader`，采样率使用常量 `spkSampleRate = 48000`。

## Proposed Changes

### 1) 新增麦克风文件写入状态变量

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`
- What：
  - 新增 `micFd`、`micDataBytes`、`micWavPath` 三个变量，和 `spk` 变量并列管理。
- Why：
  - 与现有 `spkBuffer` 落盘实现保持一致，降低改动风险。
- How：
  - 在文件顶部常量/状态区声明变量，初始值分别为 `null`、`0`、`''`。

### 2) 录制启动前创建 mic WAV 文件并写入占位头

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`
- What：
  - 在创建 `spk` 文件的同一区域，新增 `mic` 文件路径生成、`openSync`、`writeWavHeader(micFd, 0)`。
- Why：
  - 保证写入过程可顺序追加，停止时只需回填头部长度。
- How：
  - 使用同一输出目录与时间戳，文件名格式建议：`mic_progress_<timestamp>.wav`。
  - 启动日志新增 `mic wav path` 输出。

### 3) 在进度回调中追加写入 `progress.micBuffer`

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`
- What：
  - 在 `listenRecordingProgress` 内新增 mic 写入分支：
    - 条件：`micFd != null && progress.micBuffer?.byteLength > 0`
    - 写入：`fs.writeSync(micFd, ...)`
    - 累计：`micDataBytes += chunk.length`
- Why：
  - 与 `spkBuffer` 保持一致的“分片追加写”策略，便于定位录音链路问题。
- How：
  - 沿用 `try/catch` 结构，错误日志关键词使用 `micBuffer 写入失败`，便于日志筛选。

### 4) 停止时补齐 mic 收尾逻辑

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`
- What：
  - 在 `stopRecording` 后延时收尾区域，新增 mic 头回填与句柄关闭逻辑。
- Why：
  - 没有回填头部会导致播放器时长异常或无法正常识别。
- How：
  - 若 `micFd != null`：
    - 调用 `writeWavHeader(micFd, micDataBytes)`
    - `fs.closeSync(micFd)`
    - 输出 `mic wav finalized` 日志
    - `finally` 中置 `micFd = null`

### 5) 日志与可观测性增强（脚本层）

- 文件：`e:\Code\HaierGit\perception-audio-recorder\test.js`
- What：
  - 在 progress 日志中保持现有 `micChunkBytes` 字段输出，并在最终日志输出 mic 文件累计字节。
- Why：
  - 便于快速核对“回调是否持续有 mic 数据”与“本地文件是否完整落盘”。
- How：
  - 不增加新 API，只增强脚本日志。

## Assumptions & Decisions

- 决策：
  - `micBuffer` 保存格式采用 WAV（与 `spkBuffer` 一致），不新增原始 `.pcm` 分支。
  - 仅改测试脚本，不触碰 Rust 录音实现与进度结构。
- 假设：
  - `progress.micBuffer` 为 16-bit little-endian PCM 双声道分片（与当前脚本对 `spkBuffer` 的处理方式一致）。
  - 当前 `spkSampleRate = 48000` 作为 WAV 头采样率在本次改造中沿用到 mic 文件，优先完成“可落盘可回放”的目标。
- 风险说明：
  - 若实际会话采样率并非 48000，mic WAV 可能出现播放时长/速度偏差；该问题属于采样率元信息暴露与脚本头信息对齐的后续优化项。

## Verification Steps

1. 构建与运行
   - 执行 `npm run build`
   - 执行 `node test.js`
2. 录制过程验证
   - 观察日志输出 `mic wav path:`，并在 `listenRecordingProgress` 中看到 `micChunkBytes` 持续变化（有讲话时应明显增长）。
3. 停止收尾验证
   - 停止后日志出现 `mic wav finalized: <path> bytes= <N>`。
   - 本地生成 `mic_progress_<timestamp>.wav` 文件，文件大小大于 44 字节。
4. 回放验证
   - 使用播放器打开 mic 文件，确认可播放且有麦克风内容。
   - 若出现“时长/速度不正常”，记录日志中的会话采样率信息并进入后续采样率头信息对齐优化。
