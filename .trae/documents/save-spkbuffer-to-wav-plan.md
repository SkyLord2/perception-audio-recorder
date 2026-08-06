# test.js 保存 progress.spkBuffer 到本地 WAV 的实施计划

## Summary

- 目标：在 `test.js` 的进度回调中，将 `progress.spkBuffer` 持续写入本地 `WAV` 文件。
- 格式：`WAV`（16-bit PCM, little-endian）。
- 保存位置：跟随当前录制目录策略（优先 `saveDir`，否则回退到脚本当前工作目录）。
- 范围：仅调整 `test.js` 示例逻辑，不改 Rust 插件接口。

## Current State Analysis

- 当前 `test.js` 在 `listenRecordingProgress` 里仅打印 `progress.spkBuffer.byteLength`，未落盘。
- 当前已存在目录决策变量：
  - `preferredDir = path.join(os.homedir(), 'Music')`
  - `saveDir = fs.existsSync(preferredDir) ? preferredDir : undefined`
- 当前调用 `startRecording(saveDir)`，与“跟随 saveDir”策略一致。
- 进度数据类型已在 `index.d.ts` 确认：
  - `spkBuffer: Uint8Array`
  - `totalDuration: number`、`totalSize: number`

## Proposed Changes

### 1) `test.js` 新增 WAV 文件写入上下文

- 新增状态变量：
  - `spkWavPath`：扬声器 WAV 输出路径。
  - `spkFd`：文件描述符（用于随机写 header 与追加 data）。
  - `spkDataBytes`：累计写入的 PCM 数据字节数。
  - `spkChannels = 2`、`spkSampleRate = 48000`、`spkBitsPerSample = 16`（与当前录制链路保持一致）。
- 启动时构造路径：
  - 输出目录 `outputDir = saveDir ?? process.cwd()`
  - 文件名建议 `spk_progress_<timestamp>.wav`

### 2) `test.js` 新增 WAV 头写入与回填函数

- `writeWavHeader(fd, dataBytes)`：
  - 写 44 字节 PCM WAV 头。
  - 使用 `dataBytes` 填充 `ChunkSize` 与 `Subchunk2Size`。
- 初始化阶段先写占位头（`dataBytes = 0`）。
- 停止录制后回填真实头（`dataBytes = spkDataBytes`），确保文件可播放。

### 3) `listenRecordingProgress` 中追加写入 `spkBuffer`

- 在回调中：
  - 将 `progress.spkBuffer` 转为 `Buffer` 后 `fs.writeSync` 追加写入文件。
  - 累加 `spkDataBytes += progress.spkBuffer.byteLength`。
  - 保留现有日志打印（可增加 `spkWavPath` 与累计大小日志）。
- 异常处理：
  - 单次写入失败打印错误，不影响主流程继续接收事件。

### 4) 在停止流程中安全收尾

- 在 `stopRecording()` 后的回调中执行：
  - 回填 WAV 头（使用最终 `spkDataBytes`）。
  - 关闭 `spkFd`。
  - 打印输出文件路径与累计大小，便于验证。
- 防重复收尾：
  - 通过 `if (spkFd != null)` 防止重复关闭或重复回填。

### 5) 控制台验证输出增强（可读性）

- 增加关键日志：
  - `spk wav path`
  - `spk data bytes appended`
  - `spk wav finalized`

## Assumptions & Decisions

- `progress.spkBuffer` 视为与当前录制链路一致的 `PCM16LE` 数据分片，可直接拼接成 WAV `data` 块。
- WAV 参数固定采用当前工程录制参数（`48kHz / 2ch / 16bit`），与 Rust 侧配置一致。
- 不做“断电恢复”与“异常退出补头”增强，本次仅保证正常 stop 流程下可播放。
- 不修改插件导出类型与 Rust 逻辑，仅在示例脚本完成本地落盘。

## Verification Steps

1. 运行 `node test.js` 启动录制。
2. 检查控制台打印，确认 `spk_progress_*.wav` 路径已创建。
3. 录制过程中确认 `spkDataBytes` 持续增长。
4. 停止后确认日志显示 `wav finalized` 且文件已关闭。
5. 使用系统播放器打开导出的 `spk_progress_*.wav`，确认可正常播放。
6. 回归检查：原有暂停/恢复/停止与进度打印逻辑不受影响。
