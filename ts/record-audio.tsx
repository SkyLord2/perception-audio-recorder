import { UnCareType } from '@/renderer/types'
import type { RecordOptions } from '@shared/types'
import { merge } from 'lodash-es'
import { useEffect, useRef } from 'react'

export function RecordAudio() {
  const appRecordIdRef = useRef<string | null>(null)
  const idRef = useRef<string | null>(null)
  const streamRef = useRef<MediaStream | null>(null)
  const audioTotalSizeRef = useRef<number>(0)
  const audioContextRef = useRef<AudioContext | null>(null)
  const audioWorkletNodeRef = useRef<AudioWorkletNode | null>(null)
  const sourceNodeRef = useRef<MediaStreamAudioSourceNode | null>(null)

  // 重新设计的计时系统
  const recordingSessionsRef = useRef<
    Array<{
      start: number
      end?: number
    }>
  >([])
  const currentSessionStartRef = useRef<number | null>(null)
  const pauseRef = useRef<boolean>(false)
  const isRecordingRef = useRef<boolean>(false)

  const getDuration = (): number => {
    let totalDuration = 0

    // 计算所有已完成的录音会话时长
    for (const session of recordingSessionsRef.current) {
      if (session.end) {
        totalDuration += session.end - session.start
      }
    }

    // 如果当前有正在进行的录音会话，加上当前会话的时长
    if (currentSessionStartRef.current && isRecordingRef.current && !pauseRef.current) {
      totalDuration += Date.now() - currentSessionStartRef.current
    }

    return totalDuration
  }

  // 处理音频buffer数据的函数（处理原始Float32Array数据）
  const processAudioBuffer = (float32Data: Float32Array): ArrayBuffer | null => {
    try {
      // 验证Float32Array的值是否在合理范围内（音频数据通常在-1到1之间）
      let isValidRange = true
      let maxValue = 0
      for (let i = 0; i < Math.min(float32Data.length, 100); i++) {
        // 只检查前100个值以提高性能
        const absValue = Math.abs(float32Data[i])
        if (absValue > maxValue) maxValue = absValue
        if (absValue > 10) {
          // 允许一些容错范围
          console.log('发现超出范围的值:', float32Data[i], '位置:', i)
          isValidRange = false
          break
        }
      }

      // 如果验证通过，返回Float32Array的ArrayBuffer
      if (isValidRange && float32Data.length > 0) {
        return float32Data.buffer.slice() as ArrayBuffer
      } else {
        return null
      }
    } catch (error) {
      window.electron.app.writeLog(`processAudioBuffer 错误: ${error}`, 'error')
      return null
    }
  }

  // 开始录音
  const startRecordAudio = async (appRecordId: string, id: string, options?: RecordOptions) => {
    try {
      console.log('开始录音流程...')

      // 单例，同时只能有一个录音进行
      if (streamRef.current || audioContextRef.current) {
        console.warn('录音已在进行中，先停止当前录音')
        // 先清理现有资源
        await cleanupResources()
      }

      const userMediaOptions: MediaStreamConstraints = merge(
        {},
        {
          audio: {
            channelCount: 1,
            sampleRate: 16000,
            echoCancellation: true,
            noiseSuppression: true,
            autoGainControl: true,
          },
        },
        options?.mediaOptions,
      )

      // 获取麦克风权限和音频流
      const stream = await navigator.mediaDevices.getUserMedia(userMediaOptions)

      if (!stream || !stream.getAudioTracks().length) {
        throw new Error('无法获取音频流或音频轨道')
      }

      appRecordIdRef.current = appRecordId
      idRef.current = id
      streamRef.current = stream

      // 重置计时系统
      recordingSessionsRef.current = []
      currentSessionStartRef.current = Date.now()
      pauseRef.current = false
      isRecordingRef.current = false

      // 创建AudioContext用于获取原始音频数据
      const sampleRate =
        typeof userMediaOptions.audio === 'object' && userMediaOptions.audio
          ? (userMediaOptions.audio as any).sampleRate || 16000
          : 16000

      const audioContext = new (window.AudioContext || (window as any).webkitAudioContext)({
        sampleRate,
      })

      // 检查AudioContext状态
      if (audioContext.state === 'suspended') {
        await audioContext.resume()
      }
      audioContextRef.current = audioContext

      // 加载AudioWorklet模块
      console.log('加载AudioWorklet模块...')
      // 使用CDN URL加载AudioWorklet处理器
      const workletPath =
        'https://r.haier.net/assets/overlay/dts-fe/html-entries-daily/ai-client-login-page/audio-worklet-node-1128.js' // 您需要将此URL替换为实际的CDN地址
      console.log('AudioWorklet路径:', workletPath)
      await audioContext.audioWorklet.addModule(workletPath)

      const source = audioContext.createMediaStreamSource(stream)
      sourceNodeRef.current = source

      const audioWorkletNode = new AudioWorkletNode(audioContext, 'audio-record-processor')
      audioWorkletNodeRef.current = audioWorkletNode

      // 监听来自AudioWorklet的消息
      audioWorkletNode.port.onmessage = (event) => {
        try {
          if (!isRecordingRef.current || pauseRef.current) {
            return
          }

          const { type, data } = event.data

          if (type === 'audioData' && data) {
            // 获取当前录制时长
            const duration = getDuration()

            // 处理音频数据
            const processedBuffer = processAudioBuffer(data)

            if (processedBuffer) {
              // 更新总大小（近似值）
              audioTotalSizeRef.current += processedBuffer.byteLength

              // 发送录制进度事件
              window.electron.h5Api.sendRecordingProgress({
                isRecording: isRecordingRef.current,
                paused: pauseRef.current,
                totalSize: audioTotalSizeRef.current,
                totalDuration: duration,
                buffer: processedBuffer,
                appRecordId: appRecordIdRef.current || '',
                id: idRef.current || '',
              })
            }
          }
        } catch (audioProcessError: UnCareType) {
          window.electron.app.writeLog(`音频处理错误: ${audioProcessError.message}`, 'log')
        }
      }

      // 连接音频节点
      source.connect(audioWorkletNode)
      audioWorkletNode.connect(audioContext.destination)

      // 开始录音
      isRecordingRef.current = true
      window.electron.h5Api.sendAudioIsRecording(true)
      window.electron.app.writeLog('录音开始成功', 'log')
    } catch (error: UnCareType) {
      console.error('开始录音失败:', error)

      // 详细的错误处理
      let errorMessage = '未知错误'
      if (error.name === 'NotAllowedError') {
        errorMessage = '用户拒绝了麦克风权限'
      } else if (error.name === 'NotFoundError') {
        errorMessage = '未找到音频输入设备'
      } else if (error.name === 'NotSupportedError') {
        errorMessage = '浏览器不支持音频录制'
      } else if (error.name === 'NotReadableError') {
        errorMessage = '音频设备被其他应用占用'
      } else if (error.message) {
        errorMessage = error.message
      }

      window.electron.app.writeLog(`录音开始失败: ${errorMessage}`, 'error')

      // 清理可能已创建的资源
      await cleanupResources()

      // 重新抛出错误以便上层处理
      throw new Error(`录音开始失败: ${errorMessage}`)
    }
  }

  // 资源清理函数
  const cleanupResources = async () => {
    try {
      console.log('清理音频资源...')

      // 清理音频流
      if (streamRef.current) {
        streamRef.current.getTracks().forEach((track) => {
          track.stop()
          console.log('音频轨道已停止:', track.kind)
        })
        streamRef.current = null
      }

      // 清理AudioContext资源
      if (sourceNodeRef.current) {
        try {
          sourceNodeRef.current.disconnect()
        } catch (e) {
          console.warn('断开源节点连接时出错:', e)
        }
        sourceNodeRef.current = null
      }

      if (audioWorkletNodeRef.current) {
        try {
          audioWorkletNodeRef.current.disconnect()
          audioWorkletNodeRef.current.port.onmessage = null // 清理事件处理器
        } catch (e) {
          console.warn('断开AudioWorkletNode连接时出错:', e)
        }
        audioWorkletNodeRef.current = null
      }

      if (audioContextRef.current) {
        try {
          if (audioContextRef.current.state !== 'closed') {
            await audioContextRef.current.close()
            console.log('AudioContext已关闭')
          }
        } catch (e) {
          console.warn('关闭AudioContext时出错:', e)
        }
        audioContextRef.current = null
      }

      // 重置状态
      isRecordingRef.current = false
      pauseRef.current = false

      console.log('音频资源清理完成')
    } catch (error) {
      console.error('清理资源时发生错误:', error)
    }
  }

  // 暂停录音
  const pauseRecordAudio = (appRecordId: string) => {
    // 如果当前的appRecordId和传入的appRecordId不同，则不做处理，无权操作别人的录音场景
    if (appRecordId !== appRecordIdRef.current) {
      return
    }
    if (isRecordingRef.current && !pauseRef.current && sourceNodeRef.current && audioWorkletNodeRef.current) {
      // 结束当前录音会话
      if (currentSessionStartRef.current) {
        recordingSessionsRef.current.push({
          start: currentSessionStartRef.current,
          end: Date.now(),
        })
        currentSessionStartRef.current = null
      }

      // 真正暂停音频处理：断开音频节点连接
      try {
        sourceNodeRef.current.disconnect(audioWorkletNodeRef.current)
        console.log('音频节点已断开连接')
      } catch (error) {
        console.warn('断开音频节点连接时出错:', error)
      }

      pauseRef.current = true
      window.electron.h5Api.sendAudioIsPaused(true)
      window.electron.app.writeLog('录音已暂停', 'log')
    }
  }

  // 恢复录音
  const resumeRecordAudio = (appRecordId: string) => {
    // 如果当前的appRecordId和传入的appRecordId不同，则不做处理，无权操作别人的录音场景
    if (appRecordId !== appRecordIdRef.current) {
      return
    }

    if (isRecordingRef.current && pauseRef.current && sourceNodeRef.current && audioWorkletNodeRef.current) {
      // 真正恢复音频处理：重新连接音频节点
      try {
        sourceNodeRef.current.connect(audioWorkletNodeRef.current)
        console.log('音频节点已重新连接')
      } catch (error) {
        console.warn('重新连接音频节点时出错:', error)
      }

      // 开始新的录音会话
      currentSessionStartRef.current = Date.now()
      pauseRef.current = false
      window.electron.h5Api.sendAudioIsPaused(false)
      window.electron.app.writeLog('录音已恢复', 'log')
    }
  }

  const forceStopRecordAudio = async () => {
    try {
      console.log('停止录音...')

      // 结束当前录音会话（如果存在）
      if (currentSessionStartRef.current && isRecordingRef.current && !pauseRef.current) {
        recordingSessionsRef.current.push({
          start: currentSessionStartRef.current,
          end: Date.now(),
        })
        currentSessionStartRef.current = null
      }
      // 使用统一的资源清理函数
      await cleanupResources()

      // 停止录音状态
      window.electron.h5Api.sendAudioIsRecording(false)
      window.electron.app.writeLog('录音已停止', 'log')
      console.log('录音停止成功')
    } catch (error) {
      console.error('停止录音时发生错误:', error)
      window.electron.app.writeLog(`停止录音错误: ${error}`, 'error')
    }
  }

  // 停止录音
  const stopRecordAudio = async (appRecordId: string) => {
    // 如果当前的appRecordId和传入的appRecordId不同，则不做处理，无权操作别人的录音场景
    if (appRecordId !== appRecordIdRef.current) {
      return
    }

    forceStopRecordAudio()
  }

  useEffect(() => {
    const unsubStart = window.electron.h5Api.listenStartRecordingAudio((appRecordId, id, options) => {
      startRecordAudio(appRecordId, id, options)
    })
    const unsubStop = window.electron.h5Api.listenStopRecordingAudio((appRecordId) => {
      stopRecordAudio(appRecordId)
    })
    const unsubPause = window.electron.h5Api.listenPauseRecordingAudio((appRecordId: string) => {
      pauseRecordAudio(appRecordId)
    })
    const unsubResume = window.electron.h5Api.listenResumeRecordingAudio((appRecordId: string) => {
      resumeRecordAudio(appRecordId)
    })
    const unsubLogout = window.electron.business.listen4LogoutSuccess(() => {
      // 退出登录时停止录音
      forceStopRecordAudio()
    })

    return () => {
      unsubStart()
      unsubStop()
      unsubPause()
      unsubResume()
      unsubLogout()
    }
  }, [])

  return null
}
