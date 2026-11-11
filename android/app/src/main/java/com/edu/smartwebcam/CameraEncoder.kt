package com.edu.smartwebcam

import android.annotation.SuppressLint
import android.content.Context
import android.hardware.camera2.CameraCaptureSession
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraDevice
import android.hardware.camera2.CameraManager
import android.hardware.camera2.CaptureRequest
import android.hardware.camera2.CaptureResult
import android.hardware.camera2.TotalCaptureResult
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.HandlerThread
import android.os.SystemClock
import android.util.Log
import android.util.Range
import android.util.Size
import android.view.Surface
import org.json.JSONArray
import org.json.JSONObject
import kotlin.math.abs

private const val TAG = "CameraEncoder"
private const val MIME = MediaFormat.MIMETYPE_VIDEO_AVC

data class StreamConfig(val cameraId: String, val width: Int, val height: Int, val fps: Int, val bitrate: Int)

data class StartedInfo(
    val cameraId: String, val width: Int, val height: Int, val fps: Int, val bitrate: Int,
    val orientation: Int, val facing: String,
)

interface EncoderListener {
    fun onStarted(info: StartedInfo)
    fun onPacket(ptsUs: Long, flags: Int, data: ByteArray)
    fun onError(message: String)
    /** Once a second: camera timestamp → encoder output delay, in ms. */
    fun onStats(encodeAvgMs: Double, encodeMaxMs: Double) {}
}

/**
 * Camera2 capture straight into a hardware H.264 encoder's input Surface.
 * All camera/codec work happens on one HandlerThread, so calls are naturally serialized.
 */
class CameraEncoder(context: Context, private val listener: EncoderListener) {
    private val manager = context.getSystemService(CameraManager::class.java)
    private val thread = HandlerThread("camera-encoder").apply { start() }
    private val handler = Handler(thread.looper)

    private var generation = 0
    private var camera: CameraDevice? = null
    private var session: CameraCaptureSession? = null
    private var codec: MediaCodec? = null
    private var surface: Surface? = null
    private var request: CaptureRequest.Builder? = null
    private var codecConfig: ByteArray? = null
    private var torch = false

    /** What the client asked for; while set we keep (re)trying to stream it. */
    private var wanted: StreamConfig? = null
    private var activeCameraId: String? = null
    private var retryPending = false

    // Latency accounting (encoder thread only).
    private var realtimeClock = false
    private var lagSumUs = 0L
    private var lagMaxUs = 0L
    private var lagCount = 0
    private var camLagSumUs = 0L
    private var camLagCount = 0
    private val captureCallback = object : CameraCaptureSession.CaptureCallback() {
        override fun onCaptureCompleted(session: CameraCaptureSession, request: CaptureRequest, result: TotalCaptureResult) {
            val ts = result.get(CaptureResult.SENSOR_TIMESTAMP) ?: return
            val now = if (realtimeClock) SystemClock.elapsedRealtimeNanos() else System.nanoTime()
            val lag = (now - ts) / 1000
            if (lag in 0..2_000_000) { camLagSumUs += lag; camLagCount++ }
        }
    }
    private val statsTick = object : Runnable {
        override fun run() {
            if (codec == null) return
            if (lagCount > 0) listener.onStats(lagSumUs / lagCount / 1000.0, lagMaxUs / 1000.0)
            if (camLagCount > 0 && lagCount > 0) Log.d(TAG, "latency: capture result ${camLagSumUs / camLagCount / 1000} ms, encoded ${lagSumUs / lagCount / 1000} ms")
            camLagSumUs = 0; camLagCount = 0
            lagSumUs = 0; lagMaxUs = 0; lagCount = 0
            handler.postDelayed(this, 1000)
        }
    }

    // Another client (e.g. face unlock) can evict us; resume as soon as the camera is free again.
    private val availability = object : CameraManager.AvailabilityCallback() {
        override fun onCameraAvailable(cameraId: String) {
            if (wanted != null && camera == null && cameraId == activeCameraId) scheduleRetry(300)
        }
    }

    init {
        manager.registerAvailabilityCallback(availability, handler)
    }

    fun start(cfg: StreamConfig) = handler.post { wanted = cfg; stopInternal(); startInternal(cfg) }

    fun stop() = handler.post { wanted = null; stopInternal() }

    fun release() {
        handler.post {
            wanted = null
            stopInternal()
            manager.unregisterAvailabilityCallback(availability)
            thread.quitSafely()
        }
    }

    private fun cameraLost(message: String) {
        listener.onError(message)
        stopInternal()
        scheduleRetry(2000)
    }

    private fun scheduleRetry(delayMs: Long) {
        if (retryPending || wanted == null) return
        retryPending = true
        handler.postDelayed({
            retryPending = false
            val w = wanted
            if (w != null && camera == null) {
                Log.i(TAG, "retrying camera ${w.cameraId}")
                stopInternal()
                startInternal(w)
            }
        }, delayMs)
    }

    fun setTorch(on: Boolean) = handler.post {
        torch = on
        applyRepeating()
    }

    fun setBitrate(bps: Int) = handler.post {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_VIDEO_BITRATE, bps) })
    }

    fun requestKeyframe() = handler.post {
        codec?.setParameters(Bundle().apply { putInt(MediaCodec.PARAMETER_KEY_REQUEST_SYNC_FRAME, 0) })
    }

    private fun stopInternal() {
        generation++
        runCatching { session?.close() }
        runCatching { camera?.close() }
        runCatching { codec?.stop() }
        runCatching { codec?.release() }
        runCatching { surface?.release() }
        session = null; camera = null; codec = null; surface = null; request = null
        codecConfig = null
    }

    @SuppressLint("MissingPermission")
    private fun startInternal(requested: StreamConfig) {
        val gen = generation
        try {
            val ids = manager.cameraIdList
            val cameraId = if (requested.cameraId in ids) requested.cameraId else ids.first()
            activeCameraId = cameraId
            val chars = manager.getCameraCharacteristics(cameraId)
            val size = pickSize(chars, requested.width, requested.height)
            val fpsRange = pickFpsRange(chars, requested.fps)
            val fps = fpsRange.upper
            val orientation = chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0
            realtimeClock = chars.get(CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE) ==
                CameraCharacteristics.SENSOR_INFO_TIMESTAMP_SOURCE_REALTIME
            val facing = facingName(chars)

            val enc = createEncoder(size, fps, requested.bitrate)
            codec = enc
            val input = enc.createInputSurface()
            surface = input
            enc.start()
            handler.removeCallbacks(statsTick)
            handler.postDelayed(statsTick, 1000)

            manager.openCamera(cameraId, object : CameraDevice.StateCallback() {
                override fun onOpened(device: CameraDevice) {
                    if (gen != generation) { device.close(); return }
                    camera = device
                    @Suppress("DEPRECATION")
                    device.createCaptureSession(listOf(input), object : CameraCaptureSession.StateCallback() {
                        override fun onConfigured(s: CameraCaptureSession) {
                            if (gen != generation) { s.close(); return }
                            session = s
                            // RECORD and PREVIEW measured the same delay here (~105 ms sensor → frame); RECORD looks better.
                            request = device.createCaptureRequest(CameraDevice.TEMPLATE_RECORD).apply {
                                addTarget(input)
                                set(CaptureRequest.CONTROL_MODE, CaptureRequest.CONTROL_MODE_AUTO)
                                set(CaptureRequest.CONTROL_AE_TARGET_FPS_RANGE, fpsRange)
                                val afModes = chars.get(CameraCharacteristics.CONTROL_AF_AVAILABLE_MODES) ?: intArrayOf()
                                if (CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO in afModes) {
                                    set(CaptureRequest.CONTROL_AF_MODE, CaptureRequest.CONTROL_AF_MODE_CONTINUOUS_VIDEO)
                                }
                                val stab = chars.get(CameraCharacteristics.CONTROL_AVAILABLE_VIDEO_STABILIZATION_MODES) ?: intArrayOf()
                                if (CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_OFF in stab) {
                                    // Stabilization adds latency and crops; a webcam usually sits still.
                                    set(CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE, CaptureRequest.CONTROL_VIDEO_STABILIZATION_MODE_OFF)
                                }
                            }
                            applyRepeating()
                            listener.onStarted(
                                StartedInfo(cameraId, size.width, size.height, fps, requested.bitrate, orientation, facing)
                            )
                        }

                        override fun onConfigureFailed(s: CameraCaptureSession) {
                            if (gen == generation) cameraLost("camera session configuration failed")
                        }
                    }, handler)
                }

                override fun onDisconnected(device: CameraDevice) {
                    device.close()
                    if (gen == generation) cameraLost("camera taken by another app; will resume when it is free")
                }

                override fun onError(device: CameraDevice, error: Int) {
                    device.close()
                    if (gen == generation) cameraLost(when (error) {
                        CameraDevice.StateCallback.ERROR_CAMERA_IN_USE, CameraDevice.StateCallback.ERROR_MAX_CAMERAS_IN_USE -> "camera in use by another app; will resume when it is free"
                        CameraDevice.StateCallback.ERROR_CAMERA_DISABLED -> "camera disabled by device policy"
                        else -> "camera error $error"
                    })
                }
            }, handler)
        } catch (e: Exception) {
            Log.e(TAG, "start failed", e)
            cameraLost("camera start failed: ${e.message}")
        }
    }

    private fun applyRepeating() {
        val s = session ?: return
        val r = request ?: return
        r.set(CaptureRequest.FLASH_MODE, if (torch) CaptureRequest.FLASH_MODE_TORCH else CaptureRequest.FLASH_MODE_OFF)
        runCatching { s.setRepeatingRequest(r.build(), captureCallback, handler) }
            .onFailure { Log.w(TAG, "setRepeatingRequest failed", it) }
    }

    private fun createEncoder(size: Size, fps: Int, bitrate: Int, tuned: Boolean = true): MediaCodec {
        val codec = MediaCodec.createEncoderByType(MIME)
        val caps = codec.codecInfo.getCapabilitiesForType(MIME)
        codec.setCallback(object : MediaCodec.Callback() {
            override fun onInputBufferAvailable(c: MediaCodec, index: Int) {}

            override fun onOutputBufferAvailable(c: MediaCodec, index: Int, info: MediaCodec.BufferInfo) {
                try {
                    if (c !== this@CameraEncoder.codec) return
                    val buf = c.getOutputBuffer(index) ?: return
                    if (info.size > 0) {
                        buf.position(info.offset)
                        buf.limit(info.offset + info.size)
                        val data = ByteArray(info.size).also { buf.get(it) }
                        when {
                            info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0 -> {
                                codecConfig = data
                                listener.onPacket(info.presentationTimeUs, Protocol.FLAG_CONFIG, data)
                            }
                            info.flags and MediaCodec.BUFFER_FLAG_KEY_FRAME != 0 -> {
                                val cfg = codecConfig
                                val out = if (cfg != null && !startsWithSps(data)) cfg + data else data
                                listener.onPacket(info.presentationTimeUs, Protocol.FLAG_KEYFRAME, out)
                            }
                            else -> listener.onPacket(info.presentationTimeUs, 0, data)
                        }
                        if (info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG == 0) {
                            val lag = System.nanoTime() / 1000 - info.presentationTimeUs
                            if (lag in 0..2_000_000) {
                                lagSumUs += lag; lagCount++
                                if (lag > lagMaxUs) lagMaxUs = lag
                            }
                        }
                    }
                } finally {
                    runCatching { c.releaseOutputBuffer(index, false) }
                }
            }

            override fun onError(c: MediaCodec, e: MediaCodec.CodecException) {
                Log.e(TAG, "codec error", e)
                if (c === this@CameraEncoder.codec) cameraLost("encoder error: ${e.diagnosticInfo}")
            }

            override fun onOutputFormatChanged(c: MediaCodec, format: MediaFormat) {}
        }, handler)

        fun format(optional: Boolean) = MediaFormat.createVideoFormat(MIME, size.width, size.height).apply {
            setInteger(MediaFormat.KEY_COLOR_FORMAT, MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface)
            setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
            setInteger(MediaFormat.KEY_FRAME_RATE, fps)
            setInteger(MediaFormat.KEY_I_FRAME_INTERVAL, 2)
            if (optional) {
                setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
                setInteger(MediaFormat.KEY_LEVEL, avcLevel(size, fps))
                if (caps.encoderCapabilities.isBitrateModeSupported(MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)) {
                    setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
                }
                setInteger(MediaFormat.KEY_PRIORITY, 0)
                setInteger(MediaFormat.KEY_OPERATING_RATE, fps)
                if (Build.VERSION.SDK_INT >= 29) setInteger(MediaFormat.KEY_PREPEND_HEADER_TO_SYNC_FRAMES, 1)
                if (Build.VERSION.SDK_INT >= 30) setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
            }
        }

        try {
            codec.configure(format(tuned), null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
        } catch (e: Exception) {
            codec.release()
            if (!tuned) throw e
            Log.w(TAG, "configure with tuning keys failed, retrying plain", e)
            return createEncoder(size, fps, bitrate, tuned = false)
        }
        return codec
    }

    private fun pickSize(chars: CameraCharacteristics, w: Int, h: Int): Size {
        val sizes = encoderSizes(chars)
        require(sizes.isNotEmpty()) { "no encoder-compatible sizes" }
        sizes.firstOrNull { it.width == w && it.height == h }?.let { return it }
        val aspect = w.toDouble() / h
        // Same aspect ratio, closest area; otherwise closest area overall.
        return sizes.minBy {
            val aspectPenalty = if (abs(it.width.toDouble() / it.height - aspect) < 0.02) 0.0 else 1e12
            aspectPenalty + abs(it.width.toLong() * it.height - w.toLong() * h).toDouble()
        }
    }

    private fun pickFpsRange(chars: CameraCharacteristics, fps: Int): Range<Int> {
        val ranges = chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES) ?: return Range(fps, fps)
        // Prefer a fixed range at the requested rate, then the highest lower bound with that upper.
        val candidates = ranges.filter { it.upper == fps }.ifEmpty {
            val best = ranges.map { it.upper }.minBy { abs(it - fps) }
            ranges.filter { it.upper == best }
        }
        return candidates.maxBy { it.lower }
    }

    private fun startsWithSps(data: ByteArray): Boolean {
        // Annex-B: 00 00 00 01 or 00 00 01, then NAL header; type 7 = SPS.
        val i = when {
            data.size > 4 && data[0].toInt() == 0 && data[1].toInt() == 0 && data[2].toInt() == 0 && data[3].toInt() == 1 -> 4
            data.size > 3 && data[0].toInt() == 0 && data[1].toInt() == 0 && data[2].toInt() == 1 -> 3
            else -> return false
        }
        return data[i].toInt() and 0x1f == 7
    }

    companion object {
        private val encoderCaps: MediaCodecInfo.VideoCapabilities? by lazy {
            MediaCodecList(MediaCodecList.REGULAR_CODECS).codecInfos
                .firstOrNull { it.isEncoder && MIME in it.supportedTypes.map { t -> t.lowercase() } }
                ?.getCapabilitiesForType(MIME)?.videoCapabilities
        }

        fun facingName(chars: CameraCharacteristics) = when (chars.get(CameraCharacteristics.LENS_FACING)) {
            CameraCharacteristics.LENS_FACING_FRONT -> "front"
            CameraCharacteristics.LENS_FACING_BACK -> "back"
            else -> "external"
        }

        fun encoderSizes(chars: CameraCharacteristics): List<Size> {
            val map = chars.get(CameraCharacteristics.SCALER_STREAM_CONFIGURATION_MAP) ?: return emptyList()
            val vc = encoderCaps
            return map.getOutputSizes(MediaCodec::class.java).orEmpty()
                .filter { it.width in 320..3840 && it.height in 240..2160 }
                .filter { vc == null || vc.isSizeSupported(it.width, it.height) }
                .sortedByDescending { it.width * it.height }
        }

        fun avcLevel(size: Size, fps: Int): Int {
            val mbps = ((size.width + 15) / 16) * ((size.height + 15) / 16) * fps
            return when {
                mbps <= 108000 -> MediaCodecInfo.CodecProfileLevel.AVCLevel31
                mbps <= 245760 -> MediaCodecInfo.CodecProfileLevel.AVCLevel4
                mbps <= 522240 -> MediaCodecInfo.CodecProfileLevel.AVCLevel42
                else -> MediaCodecInfo.CodecProfileLevel.AVCLevel51
            }
        }

        /** The `cameras` array of the `caps` message. */
        fun describeCameras(context: Context): JSONArray {
            val manager = context.getSystemService(CameraManager::class.java)
            val out = JSONArray()
            for (id in manager.cameraIdList) {
                val chars = manager.getCameraCharacteristics(id)
                val sizes = encoderSizes(chars)
                if (sizes.isEmpty()) continue
                val fps = (chars.get(CameraCharacteristics.CONTROL_AE_AVAILABLE_TARGET_FPS_RANGES) ?: arrayOf())
                    .map { it.upper }.filter { it in 10..120 }.distinct().sorted()
                out.put(JSONObject().apply {
                    put("id", id)
                    put("facing", facingName(chars))
                    put("orientation", chars.get(CameraCharacteristics.SENSOR_ORIENTATION) ?: 0)
                    put("torch", chars.get(CameraCharacteristics.FLASH_INFO_AVAILABLE) == true)
                    put("sizes", JSONArray(sizes.map { JSONArray(listOf(it.width, it.height)) }))
                    put("fps", JSONArray(fps))
                })
            }
            return out
        }
    }
}
