const {
  doInitialize,
  startRecording,
  stopRecording,
  pauseRecording,
  resumeRecording,
  isRecording,
  getRecordDuration,
  listenStartRecordingAudio,
  listenStopRecordingAudio,
  listenPauseRecordingAudio,
  listenResumeRecordingAudio,
  listenRecordingError,
  listenRecordingProgress,
} = require('./index.js')
const crypto = require('node:crypto')
const os = require('node:os')
const path = require('node:path')
const fs = require('node:fs')
const WebSocket = require('ws')

// micBuffer/spkBuffer 的测试文件声道数应与 startRecording 的 channels 参数一致。
// progress.buffer 则始终固定为双声道：[麦克风, 扬声器]。
const WAV_CHANNELS = 1
const WAV_SAMPLE_RATE = 16000
const WAV_BITS_PER_SAMPLE = 32
const ASR_BASE_WS_URL = 'wss://office-api-ast-dx.iflyaisol.com/ast/communicate/v1'
const ASR_APP_ID = '6dfb13c5'
const ASR_ACCESS_KEY_ID = '08ba9014e134f005d48a5b6b2541859f'
const ASR_ACCESS_KEY_SECRET = 'ZDE3ZTBkZjYyYTRjODA3ZjVhOWViNzhk'
const ASR_FIXED_PARAMS = {
  audio_encode: 'pcm_s16le',
  lang: 'autodialect',
  samplerate: '16000',
}
const AUDIO_FRAME_SIZE = 4096
const FRAME_INTERVAL_MS = 128

let spkFd = null
let spkDataBytes = 0
let spkWavPath = ''
let micFd = null
let micDataBytes = 0
let micWavPath = ''
let asrWs = null
let asrSessionId = null
let asrConnected = false
let asrEnding = false
let asrEndSent = false
let sendTimer = null
let sendFrameCount = 0

// class ByteQueue {
//   constructor() {
//     this.chunks = []
//     this.length = 0
//   }

//   push(chunk) {
//     if (!chunk || chunk.length === 0) {
//       return
//     }
//     this.chunks.push(chunk)
//     this.length += chunk.length
//   }

//   consume(size) {
//     if (size <= 0 || this.length < size) {
//       return null
//     }

//     const payload = Buffer.allocUnsafe(size)
//     let offset = 0

//     while (offset < size && this.chunks.length > 0) {
//       const chunk = this.chunks[0]
//       const remaining = size - offset
//       const copyLength = Math.min(chunk.length, remaining)
//       chunk.copy(payload, offset, 0, copyLength)
//       offset += copyLength

//       if (copyLength === chunk.length) {
//         this.chunks.shift()
//       } else {
//         this.chunks[0] = chunk.subarray(copyLength)
//       }
//     }

//     this.length -= size
//     return payload
//   }
// }

/**
 * 设计目的：
 * - 避免频繁 new Uint8Array + concat 带来的分配与拷贝
 * - 以块为单位 push，按需消费固定大小的切片
 * - 简单实现，主线程友好，可替代逐次 concat
 */
class ByteQueue {
  chunks = []
  totalLength = 0

  /** 当前可用字节数 */
  get length() {
    return this.totalLength
  }

  /** 追加一段字节（不会拷贝入已有缓冲，仅保存引用） */
  push(data) {
    if (data.byteLength === 0) return
    this.chunks.push(data)
    this.totalLength += data.byteLength
  }

  /**
   * 消费前 size 个字节并返回（不足则返回 null）
   * 返回的数组是新建拷贝，保证外部可独立持有
   */
  consume(size) {
    if (size <= 0 || this.totalLength < size) return null
    const out = new Uint8Array(size)
    let offset = 0
    while (offset < size) {
      const head = this.chunks[0]
      const remain = size - offset
      if (head.byteLength <= remain) {
        out.set(head, offset)
        offset += head.byteLength
        this.chunks.shift()
      } else {
        out.set(head.subarray(0, remain), offset)
        this.chunks[0] = head.subarray(remain)
        offset += remain
      }
    }
    this.totalLength -= size
    return out
  }

  /** 清空队列 */
  clear() {
    this.chunks = []
    this.totalLength = 0
  }
}

const pcmByteQueue = new ByteQueue()

function float32ArrayToBuffer(floatArray) {
  // `listenRecordingProgress` 现在返回 Float32Array。
  // 必须直接复用它的底层 ArrayBuffer，才能正确写出 IEEE754 Float32 WAV 数据。
  return Buffer.from(floatArray.buffer, floatArray.byteOffset, floatArray.byteLength)
}

function float32ToPcm16Buffer(floatArray) {
  const buffer = Buffer.allocUnsafe(floatArray.length * 2)
  for (let i = 0; i < floatArray.length; i++) {
    const sample = Math.max(-1, Math.min(1, floatArray[i]))
    const pcmValue = sample < 0 ? Math.round(sample * 0x8000) : Math.round(sample * 0x7fff)
    buffer.writeInt16LE(pcmValue, i * 2)
  }
  return buffer
}

function getMixedMicChannel(mixedBuffer) {
  // buffer 是交错双声道，偶数下标为麦克风，奇数下标为扬声器。
  if (!mixedBuffer || mixedBuffer.length === 0) {
    return new Float32Array(0)
  }

  if (mixedBuffer.length % 2 !== 0) {
    throw new Error('progress.buffer 必须包含偶数个 Float32 样本')
  }

  const micBuffer = new Float32Array(mixedBuffer.length / 2)
  for (let i = 0; i < micBuffer.length; i += 1) {
    micBuffer[i] = mixedBuffer[i * 2]
  }
  return micBuffer
}

/**
 * 将任意输入（Float32 或 Int16 PCM）转换为 16-bit PCM / 16kHz / 单声道的连续字节流
 * 注意：仅做格式与采样率转换，不做分帧，适合与环形缓冲/字节队列配合使用
 */
function convertToPCM16Bytes(buffer, options) {
  const { inputIsFloat32, inputSampleRate, targetSampleRate = 16000 } = options

  let monoFloat
  if (inputIsFloat32 || buffer.byteLength % 4 === 0) {
    monoFloat = new Float32Array(buffer)
  } else {
    const int16View = new Int16Array(buffer)
    monoFloat = new Float32Array(int16View.length)
    for (let i = 0; i < int16View.length; i += 1) {
      monoFloat[i] = int16View[i] / 0x8000
    }
  }

  const sourceRate = inputSampleRate || targetSampleRate
  let resampled
  if (sourceRate !== targetSampleRate) {
    const ratio = targetSampleRate / sourceRate
    const newLength = Math.round(monoFloat.length * ratio)
    resampled = new Float32Array(newLength)
    for (let i = 0; i < newLength; i += 1) {
      const srcIndex = i / ratio
      const i0 = Math.floor(srcIndex)
      const i1 = Math.min(i0 + 1, monoFloat.length - 1)
      const t = srcIndex - i0
      resampled[i] = (1 - t) * monoFloat[i0] + t * monoFloat[i1]
    }
  } else {
    resampled = monoFloat
  }

  const int16 = new Int16Array(resampled.length)
  for (let i = 0; i < resampled.length; i += 1) {
    let s = resampled[i]
    if (s > 1) s = 1
    if (s < -1) s = -1
    int16[i] = s < 0 ? s * 0x8000 : s * 0x7fff
  }
  return new Uint8Array(int16.buffer)
}

function writeWavHeader(fd, dataBytes) {
  const wavAudioFormat = 3 // IEEE float
  const blockAlign = (WAV_CHANNELS * WAV_BITS_PER_SAMPLE) / 8
  const byteRate = WAV_SAMPLE_RATE * blockAlign
  const header = Buffer.alloc(44)

  header.write('RIFF', 0)
  header.writeUInt32LE(36 + dataBytes, 4)
  header.write('WAVE', 8)
  header.write('fmt ', 12)
  header.writeUInt32LE(16, 16)
  header.writeUInt16LE(wavAudioFormat, 20)
  header.writeUInt16LE(WAV_CHANNELS, 22)
  header.writeUInt32LE(WAV_SAMPLE_RATE, 24)
  header.writeUInt32LE(byteRate, 28)
  header.writeUInt16LE(blockAlign, 32)
  header.writeUInt16LE(WAV_BITS_PER_SAMPLE, 34)
  header.write('data', 36)
  header.writeUInt32LE(dataBytes, 40)

  fs.writeSync(fd, header, 0, 44, 0)
}

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms))
}

function generateUuid() {
  return crypto.randomBytes(16).toString('hex')
}

function getUtcTimeForIflytek() {
  const now = new Date()
  const utcMs = now.getTime() + now.getTimezoneOffset() * 60 * 1000
  const beijingDate = new Date(utcMs + 8 * 60 * 60 * 1000)
  const pad = (value) => String(value).padStart(2, '0')
  return `${beijingDate.getFullYear()}-${pad(beijingDate.getMonth() + 1)}-${pad(
    beijingDate.getDate(),
  )}T${pad(beijingDate.getHours())}:${pad(beijingDate.getMinutes())}:${pad(beijingDate.getSeconds())}+0800`
}

function buildSortedQuery(params) {
  return Object.entries(params)
    .filter(([, value]) => value != null && String(value).trim() !== '')
    .sort(([leftKey], [rightKey]) => leftKey.localeCompare(rightKey))
    .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(String(value))}`)
    .join('&')
}

function buildIflytekWsUrl() {
  const authParams = {
    accessKeyId: ASR_ACCESS_KEY_ID,
    appId: ASR_APP_ID,
    uuid: generateUuid(),
    utc: getUtcTimeForIflytek(),
    ...ASR_FIXED_PARAMS,
  }

  const baseString = buildSortedQuery(authParams)
  const signature = crypto.createHmac('sha1', ASR_ACCESS_KEY_SECRET).update(baseString, 'utf8').digest('base64')

  const fullQuery = buildSortedQuery({
    ...authParams,
    signature,
  })

  return `${ASR_BASE_WS_URL}?${fullQuery}`
}

function extractSentenceFromIflytekResult(message) {
  if (message?.msg_type !== 'result' || message?.res_type !== 'asr') {
    return null
  }

  const rtList = message?.data?.cn?.st?.rt
  if (!Array.isArray(rtList) || rtList.length === 0) {
    return null
  }

  const words = []
  for (const rt of rtList) {
    if (!Array.isArray(rt?.ws)) {
      continue
    }

    for (const ws of rt.ws) {
      if (!Array.isArray(ws?.cw) || ws.cw.length === 0) {
        continue
      }

      // 按讯飞文档与当前返回结构，`cw` 是候选词数组，取第一个候选的 `w` 拼接句子。
      const bestWord = ws.cw[0]?.w
      if (typeof bestWord === 'string' && bestWord.length > 0) {
        words.push(bestWord)
      }
    }
  }

  if (words.length === 0) {
    return null
  }

  return {
    sentence: words.join(''),
    isLastSegment: Boolean(message?.data?.cn?.st?.ls),
    segmentId: message?.data?.seg_id,
  }
}

function handleIflytekMessage(rawData, isBinary) {
  if (isBinary) {
    console.log('[iflytek] 忽略二进制响应，bytes=', rawData.length)
    return
  }

  const text = Buffer.isBuffer(rawData) ? rawData.toString('utf8') : String(rawData)
  try {
    const message = JSON.parse(text)
    // console.log('[iflytek] 消息:', JSON.stringify(message))

    if (message.msg_type === 'action' && message.data && message.data.sessionId) {
      asrSessionId = message.data.sessionId
      console.log('[iflytek] sessionId:', asrSessionId)
    }

    const sentenceInfo = extractSentenceFromIflytekResult(message)
    if (sentenceInfo) {
      console.log(
        sentenceInfo.isLastSegment ? '[iflytek] 最终句子:' : '[iflytek] 中间句子:',
        sentenceInfo.sentence,
        `(seg_id=${sentenceInfo.segmentId ?? 'N/A'})`,
      )
    }
  } catch (error) {
    console.log('[iflytek] 非JSON文本消息:', text)
  }
}

function stopAsrSender() {
  if (sendTimer != null) {
    clearInterval(sendTimer)
    sendTimer = null
  }
}

function startAsrSender() {
  stopAsrSender()
  sendTimer = setInterval(() => {
    if (!asrConnected || !asrWs || asrWs.readyState !== WebSocket.OPEN) {
      return
    }

    const payload = pcmByteQueue.consume(AUDIO_FRAME_SIZE)
    if (!payload) {
      return
    }

    asrWs.send(payload, { binary: true }, (error) => {
      if (error) {
        console.error('[iflytek] 音频帧发送失败:', error)
        return
      }

      sendFrameCount += 1
      if (sendFrameCount % 10 === 0) {
        // console.log(
        //   '[iflytek] 已发送音频帧:',
        //   sendFrameCount,
        //   'queueBytes=',
        //   pcmByteQueue.length,
        // )
      }
    })
  }, FRAME_INTERVAL_MS)
}

function closeAsrConnection() {
  stopAsrSender()
  if (asrWs) {
    try {
      asrWs.close(1000, 'client finished')
    } catch (error) {
      console.error('[iflytek] 关闭WebSocket失败:', error)
    }
  }
}

function sendAsrEndIfNeeded() {
  if (asrEndSent || !asrWs || asrWs.readyState !== WebSocket.OPEN) {
    return
  }

  const endPayload = { end: true }
  if (asrSessionId) {
    endPayload.sessionId = asrSessionId
  }

  const endMessage = JSON.stringify(endPayload)
  asrWs.send(endMessage, (error) => {
    if (error) {
      console.error('[iflytek] 结束标记发送失败:', error)
      return
    }
    asrEndSent = true
    console.log('[iflytek] 已发送结束标记:', endMessage)
  })
}

async function drainAsrQueueAndFinish() {
  if (asrEnding) {
    return
  }
  asrEnding = true

  const deadline = Date.now() + 2000
  while (pcmByteQueue.length >= AUDIO_FRAME_SIZE && Date.now() < deadline) {
    await delay(50)
  }

  if (pcmByteQueue.length > 0) {
    console.log('[iflytek] 丢弃尾部不足一帧的PCM字节数:', pcmByteQueue.length)
  }

  sendAsrEndIfNeeded()
  await delay(2000)
  closeAsrConnection()
}

function connectIflytekAsr() {
  const wsUrl = buildIflytekWsUrl()
  console.log('[iflytek] 建立连接: appId=', ASR_APP_ID, 'samplerate=', ASR_FIXED_PARAMS.samplerate)

  return new Promise((resolve, reject) => {
    let settled = false
    const ws = new WebSocket(wsUrl, {
      handshakeTimeout: 15000,
    })
    asrWs = ws

    ws.on('open', async () => {
      asrConnected = true
      console.log('[iflytek] WebSocket握手成功，等待服务端稳定...')
      startAsrSender()
      await delay(1500)
      if (!settled) {
        settled = true
        resolve()
      }
    })

    ws.on('message', (data, isBinary) => {
      handleIflytekMessage(data, isBinary)
    })

    ws.on('error', (error) => {
      console.error('[iflytek] WebSocket异常:', error)
      if (!settled) {
        settled = true
        reject(error)
      }
    })

    ws.on('close', (code, reasonBuffer) => {
      asrConnected = false
      stopAsrSender()
      const reason = Buffer.isBuffer(reasonBuffer) ? reasonBuffer.toString('utf8') : String(reasonBuffer || '')
      console.log('[iflytek] WebSocket关闭: code=', code, 'reason=', reason)
      if (!settled) {
        settled = true
        reject(new Error(`WebSocket closed before ready: ${code} ${reason}`))
      }
    })
  })
}

function finalizeWavFiles() {
  if (micFd != null) {
    try {
      writeWavHeader(micFd, micDataBytes)
      fs.closeSync(micFd)
      console.log('mic wav finalized:', micWavPath, 'bytes=', micDataBytes)
    } catch (finalizeErr) {
      console.error('mic wav 收尾失败:', finalizeErr)
    } finally {
      micFd = null
    }
  }

  if (spkFd != null) {
    try {
      writeWavHeader(spkFd, spkDataBytes)
      fs.closeSync(spkFd)
      console.log('spk wav finalized:', spkWavPath, 'bytes=', spkDataBytes)
    } catch (finalizeErr) {
      console.error('spk wav 收尾失败:', finalizeErr)
    } finally {
      spkFd = null
    }
  }
}

doInitialize((err, log) => {
  if (err) {
    console.error('日志记录失败:', err)
    return
  }
  console.log('日志记录成功:', log)
})

listenStartRecordingAudio((err) => {
  if (err) {
    console.error('开始监听回调异常:', err)
    return
  }
  console.log('[event] start')
})

listenPauseRecordingAudio((err) => {
  if (err) {
    console.error('暂停监听回调异常:', err)
    return
  }
  console.log('[event] pause')
})

listenResumeRecordingAudio((err) => {
  if (err) {
    console.error('恢复监听回调异常:', err)
    return
  }
  console.log('[event] resume')
})

listenStopRecordingAudio((err) => {
  if (err) {
    console.error('停止监听回调异常:', err)
    return
  }
  console.log('[event] stop')
})

listenRecordingError((err, errorInfo) => {
  if (err) {
    console.error('错误监听回调异常:', err)
    return
  }
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
    console.error('进度监听回调异常:', err)
    return
  }

  if (progress.buffer && progress.buffer.length % 2 !== 0) {
    console.error('buffer 声道格式错误：固定双声道长度必须为偶数')
    return
  }

  if (spkFd != null && progress.spkBuffer?.length > 0) {
    try {
      const spkChunk = float32ArrayToBuffer(progress.spkBuffer)
      fs.writeSync(spkFd, spkChunk, 0, spkChunk.length, 44 + spkDataBytes)
      spkDataBytes += spkChunk.length
    } catch (writeErr) {
      console.error('spkBuffer 写入失败:', writeErr)
    }
  }

  if (micFd != null && progress.micBuffer?.length > 0) {
    try {
      const micChunk = float32ArrayToBuffer(progress.micBuffer)
      fs.writeSync(micFd, micChunk, 0, micChunk.length, 44 + micDataBytes)
      micDataBytes += micChunk.length
      // pcmByteQueue.push(float32ToPcm16Buffer(progress.micBuffer))
      // const pcmBytes = convertToPCM16Bytes(progress.micBuffer, {
      //   inputIsFloat32: true,
      //   inputSampleRate: 16000,
      //   targetSampleRate: 16000,
      // })
      // pcmByteQueue.push(Buffer.from(pcmBytes.buffer, pcmBytes.byteOffset, pcmBytes.byteLength))
      const pcmBytes = convertToPCM16Bytes(progress.micBuffer.buffer, {
        inputIsFloat32: progress.micBuffer.buffer.byteLength % 4 === 0,
        targetSampleRate: 16000,
      })
      pcmByteQueue.push(pcmBytes)
    } catch (writeErr) {
      console.error('micBuffer 写入失败:', writeErr)
    }
  }

  // console.log(
  //   '[progress]',
  //   'duration(ms)=',
  //   progress.totalDuration,
  //   'chunkSamples=',
  //   progress.buffer.length,
  //   'micChunkSamples=',
  //   progress.micBuffer.length,
  //   'spkChunkSamples=',
  //   progress.spkBuffer.length,
  //   'totalSize=',
  //   progress.totalSize,
  //   'bufferLayout=',
  //   '[mic, spk]',
  // )
})

async function main() {
  const preferredDir = path.join(os.homedir(), '.ai-client-test\\record\\audio\\S04728')
  const saveDir = fs.existsSync(preferredDir) ? preferredDir : undefined
  const outputDir = saveDir ?? process.cwd()
  // 仅控制 Rust 原生主录音文件是否落盘；本脚本仍会把 progress.micBuffer/spkBuffer 另存为本地 WAV。
  // 当前示例走 recordDevice=0，Rust 侧会在输出设备可用时内部尝试启用 MicOnly-AEC。
  // 即使内部启动了 render reference，progress.spkBuffer 在 MicOnly 模式下仍然应保持为空。
  const nativeSaveToFile = true
  const timestamp = new Date().toISOString().replace(/[:.]/g, '-')

  spkWavPath = path.join(outputDir, `spk_progress_${timestamp}.wav`)
  spkFd = fs.openSync(spkWavPath, 'w')
  writeWavHeader(spkFd, 0)
  console.log('spk wav path:', spkWavPath)

  micWavPath = path.join(outputDir, `mic_progress_${timestamp}.wav`)
  micFd = fs.openSync(micWavPath, 'w')
  writeWavHeader(micFd, 0)
  console.log('mic wav path:', micWavPath)

  await connectIflytekAsr()

  startRecording(saveDir, 2, 16000, 'mono', nativeSaveToFile)
  console.log('native saveToFile:', nativeSaveToFile)
  console.log('recordDevice=2: 预期 buffer 固定为 [mic, spk] 双声道，并同时输出 micBuffer/spkBuffer')
  console.log('录制启动后状态:', isRecording())

  // setTimeout(() => {
  //   console.log('执行暂停')
  //   pauseRecording()
  //   console.log('暂停后状态:', isRecording(), '时长(ms):', getRecordDuration())
  // }, 10000)

  // setTimeout(() => {
  //   console.log('执行恢复')
  //   resumeRecording()
  //   console.log('恢复后状态:', isRecording(), '时长(ms):', getRecordDuration())
  // }, 20000)

  setTimeout(
    async () => {
      console.log('执行停止')
      stopRecording()
      await drainAsrQueueAndFinish()
      finalizeWavFiles()
      console.log('停止后状态:', isRecording(), '最终时长(ms):', getRecordDuration())
    },
    1000 * 60 * 2,
  )
}

main().catch(async (error) => {
  console.error('实时转写联调失败:', error)
  await drainAsrQueueAndFinish().catch(() => {})
  finalizeWavFiles()
})
