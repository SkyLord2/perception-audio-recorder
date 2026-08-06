# 音频录制感知

## 1.1 先决条件

- 无需管理员权限。
- 需要可用的麦克风和系统输出设备（扬声器/耳机）。
- 运行环境建议为 Node.js/Electron 主进程。

## 1.2 实现原理

通过底层音频设备采集麦克风与系统输出音频流，在 Rust 侧完成重采样、混合与写盘，输出为 WAV（PCM16LE）文件。  
录制过程中通过回调实时上报状态与进度：

- 开始录制回调
- 停止录制回调
- 暂停录制回调
- 恢复录制回调
- 录音错误回调（设备不可用、无音频包超时等）
- 录制进度回调（总时长 + 合并音频块 + 麦克风音频块 + 扬声器音频块 + 累计大小）

其中进度回调中的音频块使用 `Float32Array` 上报，可在 JS 侧直接按样本数组处理；如需写文件，可再按底层 `ArrayBuffer` 转成字节视图。

采样率策略说明：

- 启动录制时会获取并对比 `mic_sr` 与 `spk_sr`，并通过 `report_info_log` 输出决策日志。
- 若 `mic_sr == spk_sr`：关闭双路重采样，WAV 采用该相同采样率。
- 若 `mic_sr != spk_sr`：以 `mic_sr` 作为 WAV 基准采样率，仅对扬声器流做单侧重采样到 `mic_sr`。
- 若仅麦克风可用：WAV 采样率使用 `mic_sr`；若仅扬声器可用：WAV 采样率使用 `spk_sr`。

## 1.3 使用指南

接入该方案来感知音频录制状态与进度，需要在项目中引入 `@aiui/perception-audio-recorder` 依赖。

```bash
npm install @aiui/perception-audio-recorder --ignore-scripts
```

注意：

- 原生插件安装建议加 `--ignore-scripts`，避免触发本地重编译导致安装失败。
- 录制文件优先输出到传入的 `saveDir`；若未传或不可用，回退到当前用户 `Music` 目录；再不可用则回退到当前工作目录。

示例代码如下：

```javascript
const {
  doInitialize,
  startRecording,
  stopRecording,
  pauseRecording,
  resumeRecording,
  isRecording,
  isPaused,
  getRecordDuration,
  listenStartRecordingAudio,
  listenStopRecordingAudio,
  listenPauseRecordingAudio,
  listenResumeRecordingAudio,
  listenRecordingError,
  listenRecordingProgress,
} = require('@aiui/perception-audio-recorder')

doInitialize((err, log) => {
  if (err) {
    console.error('初始化失败:', err)
    return
  }
  console.log('[native log]:', log)
})

listenStartRecordingAudio(() => console.log('[event] start'))
listenPauseRecordingAudio(() => console.log('[event] pause'))
listenResumeRecordingAudio(() => console.log('[event] resume'))
listenStopRecordingAudio(() => console.log('[event] stop'))
listenRecordingError((err, errorInfo) => {
  if (err) return
  console.error(
    '[recording-error]',
    errorInfo.errorType,
    errorInfo.errorCode,
    errorInfo.errorMessage,
    errorInfo.occurredAt,
  )
})

listenRecordingProgress((err, progress) => {
  if (err) {
    console.error('进度回调异常:', err)
    return
  }
  console.log(
    '[progress]',
    'duration(ms)=',
    progress.totalDuration,
    'chunkSamples=',
    progress.buffer.length,
    'micChunkSamples=',
    progress.micBuffer.length,
    'spkChunkSamples=',
    progress.spkBuffer.length,
    'totalSize=',
    progress.totalSize,
  )
})

const saveDir = require('node:fs').existsSync(require('node:path').join(require('node:os').homedir(), 'Music'))
  ? require('node:path').join(require('node:os').homedir(), 'Music')
  : undefined
startRecording(saveDir)
console.log('录制启动后状态:', isRecording())

setTimeout(() => {
  pauseRecording()
  console.log('暂停后会话状态:', isRecording(), '暂停状态:', isPaused(), '时长(ms):', getRecordDuration())
}, 10000)

setTimeout(() => {
  resumeRecording()
  console.log('恢复后会话状态:', isRecording(), '暂停状态:', isPaused(), '时长(ms):', getRecordDuration())
}, 20000)

setTimeout(() => {
  stopRecording()
  setTimeout(() => {
    console.log('停止后状态:', isRecording(), '最终时长(ms):', getRecordDuration())
  }, 2000)
}, 1000 * 60)
```

### API 列表

- `doInitialize(log): void`
- `startRecording(saveDir?: string | null, recordDevice?: number, sampleRate?: number, channels?: string, saveToFile?: boolean): void`
- `stopRecording(): void`
- `pauseRecording(): void`
- `resumeRecording(): void`
- `isRecording(): boolean`
- `isPaused(): boolean`
- `getRecordDuration(): number`
- `listenStartRecordingAudio(listener): void`
- `listenStopRecordingAudio(listener): void`
- `listenPauseRecordingAudio(listener): void`
- `listenResumeRecordingAudio(listener): void`
- `listenRecordingProgress(listener): void`
- `listenRecordingError(listener): void`

### `startRecording` 参数增强

- `saveDir`：录音文件目录，可选。
- `recordDevice`：录制设备类型，可选，默认 `2`。
  - `0`：仅录制麦克风
  - `1`：仅录制扬声器
  - `2`：同时录制麦克风和扬声器
  - 当 `recordDevice=0` 且 `AEC` 编译期开关开启时，会在内部尝试启动默认输出设备的 loopback render reference，仅供麦克风回声消除使用
- `sampleRate`：采样率，可选，默认 `16000`。
  - 合法值：`8000 | 16000 | 32000 | 48000`
  - 非法值：回退到动态采样率决策（设备探测策略）
- `channels`：声道模式，可选，默认 `"mono"`。
  - `"mono"`：单声道
  - `"stereo"`：立体声
- `saveToFile`：是否保存 Rust 侧本地录音文件，可选，默认 `true`。
  - `true`：保持现有行为，创建并持续写入本地 WAV 文件
  - `false`：仅实时采集并上报进度/时长，不创建 Rust 侧本地 WAV 文件
  - 当 `saveToFile=false` 且仍传入 `saveDir` 时，`saveDir` 会被忽略并记录日志

示例：

```javascript
startRecording(undefined, 0, 16000, 'mono') // 仅麦克风，16k，单声道；若系统输出设备可用，内部会启用 MicOnly-AEC
startRecording(undefined, 1, 48000, 'stereo') // 仅扬声器，48k，立体声
startRecording(undefined, 2, 32000, 'mono') // 双路，32k，单声道
startRecording(undefined, 0, 16000, 'mono', false) // 仅麦克风实时上报，不保存 Rust 侧本地文件
```

### `RecordingProgressInfo` 结构

- `totalDuration: number` 当前累计录制时长（毫秒）
- `buffer: Float32Array` 当前上报合并音频数据块
- `micBuffer: Float32Array` 当前上报麦克风音频数据块（经 `sonora` 3A 处理后的麦克风数据）
- `spkBuffer: Float32Array` 当前上报扬声器音频数据块
- `totalSize: number` 当前累计输出数据大小（字节）

`listenRecordingProgress` 分片规则：

- 上报目标频次：每 `40ms` 一次。
- 单次上报目标样本数：`sampleRate × channels × 0.04s`。
- 例如：`16000Hz × 1声道 × 0.04s = 640 samples`。
- `micBuffer` 为处理后的麦克风数据，不经过 PLC、缺帧补偿或补零；当麦克风真实数据不足时，允许出现小包或空包。
- 磁盘强制刷新（`writer.flush`）与上报已解耦，默认每 `1s` 执行一次安全刷新。
- 若 stop 时最后一包不足 `40ms`，会作为尾包一次性上报，避免数据丢失。
- 麦克风 3A 通过 `src/global.rs` 中的 `ENABLE_MIC_NOISE_SUPPRESSION`、`ENABLE_MIC_ECHO_CANCELLATION`、`ENABLE_MIC_AUTO_GAIN_CONTROL` 编译期开关控制。
- `recordDevice=0` 时若默认输出设备可用，会在内部启动 loopback render reference 做 MicOnly-AEC，但该参考流不会进入 `spkBuffer`。
- 当 `AEC` 开启但本次会话没有可用扬声器参考流，或内部 render reference 启动失败时，会自动降级为仅保留降噪/自动增益，不阻断录制启动。
- 当 `saveToFile=true` 时，`totalSize` 近似表示当前 Rust 侧本地录音文件总大小（含 WAV 头）。
- 当 `saveToFile=false` 时，`totalSize` 表示累计音频数据字节数，不对应磁盘文件大小。

### `RecordingErrorInfo` 结构

- `errorType: string` 错误类型（如 `NoAudioPacketTimeout`）
- `errorMessage: string` 错误描述信息
- `errorCode: number` 错误编码
- `occurredAt: string` 错误发生时间

### 容错与错误行为

- 单侧有数据包（仅麦克风或仅扬声器）时，`listenRecordingProgress` 仍持续上报；缺失侧按静音补齐。
- 当 `recordDevice=0` 时，`spkBuffer` 固定上报空数据；即使内部为了 MicOnly-AEC 启用了 render reference，该参考流也不会对外暴露。
- 当 `recordDevice=1` 时，`micBuffer` 固定上报空数据。
- 连续 5 秒双侧都无音频数据包时，会触发 `listenRecordingError`，但录制不会自动停止。
- 麦克风和扬声器都不可用时，`startRecording` 会直接抛错并触发错误回调。
- `pauseRecording` 后 `isRecording()` 仍为 `true`（会话未结束），可通过 `isPaused()` 判断当前是否处于暂停态。
- 采样率不一致时仅对扬声器做重采样；采样率一致时不做重采样。
- `channels="mono"` 时，`buffer/micBuffer/spkBuffer` 均为单声道 `Float32Array` 数据。
- 写盘文件中的麦克风侧与 `micBuffer` 保持一致，均为经过 3A 处理后的麦克风数据。
- `saveToFile=false` 时，Rust 侧不会创建主录音文件，但 `listenRecordingProgress`、时长统计、暂停/恢复与错误回调保持可用。

### `spkBuffer` 保存为 WAV（示例）

可参考 `test.js` 的完整实现：在 `listenRecordingProgress` 中将 `progress.spkBuffer` 分片追加写入，并在 `stopRecording` 后回填 WAV 头。关键步骤如下：

```javascript
const fs = require('node:fs')
let spkFd = fs.openSync('spk_progress.wav', 'w')
let spkDataBytes = 0

listenRecordingProgress((err, progress) => {
  if (err) return
  if (progress.spkBuffer?.length > 0) {
    const spkChunk = Buffer.from(
      progress.spkBuffer.buffer,
      progress.spkBuffer.byteOffset,
      progress.spkBuffer.byteLength,
    )
    fs.writeSync(spkFd, spkChunk, 0, spkChunk.length, 44 + spkDataBytes)
    spkDataBytes += spkChunk.length
  }
})
```

## 1.4 依赖包

- npm 包名：`@aiui/perception-audio-recorder`
- 发布地址：`https://nexus.haier.net/repository/dts-npm/`

安装命令：

```bash
npm install @aiui/perception-audio-recorder --ignore-scripts
```

## 1.5 electron 项目 demo

仓库内可参考以下文件进行联调：

- `test.js`：基础初始化、开始/暂停/恢复/停止和进度监听示例。
- `src/lib.rs`：录制主流程与导出接口实现。
- `src/global.rs`：进度结构体与全局监听器实现。

本地验证步骤：

```bash
npm install
npm run build
node test.js
```
