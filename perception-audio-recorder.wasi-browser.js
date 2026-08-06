import {
  createOnMessage as __wasmCreateOnMessageForFsProxy,
  getDefaultContext as __emnapiGetDefaultContext,
  instantiateNapiModuleSync as __emnapiInstantiateNapiModuleSync,
  WASI as __WASI,
} from '@napi-rs/wasm-runtime'



const __wasi = new __WASI({
  version: 'preview1',
})

const __wasmUrl = new URL('./perception-audio-recorder.wasm32-wasi.wasm', import.meta.url).href
const __emnapiContext = __emnapiGetDefaultContext()


const __sharedMemory = new WebAssembly.Memory({
  initial: 4000,
  maximum: 65536,
  shared: true,
})

const __wasmFile = await fetch(__wasmUrl).then((res) => res.arrayBuffer())

const {
  instance: __napiInstance,
  module: __wasiModule,
  napiModule: __napiModule,
} = __emnapiInstantiateNapiModuleSync(__wasmFile, {
  context: __emnapiContext,
  asyncWorkPoolSize: 4,
  wasi: __wasi,
  onCreateWorker() {
    const worker = new Worker(new URL('./wasi-worker-browser.mjs', import.meta.url), {
      type: 'module',
    })

    return worker
  },
  overwriteImports(importObject) {
    importObject.env = {
      ...importObject.env,
      ...importObject.napi,
      ...importObject.emnapi,
      memory: __sharedMemory,
    }
    return importObject
  },
  beforeInit({ instance }) {
    for (const name of Object.keys(instance.exports)) {
      if (name.startsWith('__napi_register__')) {
        instance.exports[name]()
      }
    }
  },
})
export default __napiModule.exports
export const doInitialize = __napiModule.exports.doInitialize
export const getRecordDuration = __napiModule.exports.getRecordDuration
export const isPaused = __napiModule.exports.isPaused
export const isRecording = __napiModule.exports.isRecording
export const listenPauseRecordingAudio = __napiModule.exports.listenPauseRecordingAudio
export const listenRecordingError = __napiModule.exports.listenRecordingError
export const listenRecordingProgress = __napiModule.exports.listenRecordingProgress
export const listenResumeRecordingAudio = __napiModule.exports.listenResumeRecordingAudio
export const listenStartRecordingAudio = __napiModule.exports.listenStartRecordingAudio
export const listenStopRecordingAudio = __napiModule.exports.listenStopRecordingAudio
export const pauseRecording = __napiModule.exports.pauseRecording
export const resumeRecording = __napiModule.exports.resumeRecording
export const startRecording = __napiModule.exports.startRecording
export const stopRecording = __napiModule.exports.stopRecording
