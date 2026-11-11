package com.edu.smartwebcam

import android.content.Context
import android.hardware.camera2.CameraCharacteristics
import android.hardware.camera2.CameraManager
import android.os.Build
import android.util.Log
import org.json.JSONObject
import java.io.IOException
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.CompletableFuture
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.TimeoutException
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread

private const val TAG = "StreamServer"

/** Max video packets waiting in a session's send queue before we start dropping (~0.5 s at 30 fps). */
private const val MAX_QUEUED_VIDEO = 15
private const val READ_TIMEOUT_MS = 10_000
private const val PAIRING_TIMEOUT_S = 60

interface ServerHost {
    /** Ask the user whether this PC may connect. Called from a session thread. */
    fun showPairingPrompt(request: PairingRequest)
    fun hidePairingPrompt()
    fun onStreamingChanged(streaming: Boolean)
}

/**
 * Accepts one client at a time on [Protocol.PORT], pairs it and pumps encoder output to it.
 * A newly trusted client replaces the current one.
 */
class StreamServer(private val context: Context, private val trust: TrustStore, private val host: ServerHost) : EncoderListener {
    private val encoder = CameraEncoder(context, this)
    private var serverSocket: ServerSocket? = null

    @Volatile private var active: Session? = null
    @Volatile private var pending: CompletableFuture<Boolean>? = null
    private val lock = Any()

    fun start() {
        val ss = ServerSocket().apply {
            reuseAddress = true
            bind(InetSocketAddress(Protocol.PORT))
        }
        serverSocket = ss
        thread(name = "accept", isDaemon = true) {
            while (!ss.isClosed) {
                val socket = try { ss.accept() } catch (e: IOException) { break }
                thread(name = "session-${socket.inetAddress.hostAddress}", isDaemon = true) { Session(socket).run() }
            }
        }
        AppState.setStatus(Status.Waiting)
    }

    fun stop() {
        runCatching { serverSocket?.close() }
        active?.close()
        encoder.release()
        AppState.setStatus(Status.Stopped)
    }

    fun resolvePairing(allow: Boolean) {
        pending?.complete(allow)
    }

    // ---- EncoderListener (called on the encoder thread) ----

    override fun onStarted(info: StartedInfo) {
        val s = active ?: return
        s.sendCtrl(ctrl("started",
            "camera" to info.cameraId, "w" to info.width, "h" to info.height, "fps" to info.fps,
            "bitrate" to info.bitrate, "orientation" to info.orientation, "facing" to info.facing))
        AppState.setStatus(Status.Streaming(s.pcName, info.width, info.height, info.fps, info.facing))
        host.onStreamingChanged(true)
    }

    override fun onPacket(ptsUs: Long, flags: Int, data: ByteArray) {
        active?.sendVideo(ptsUs, flags, data)
    }

    override fun onStats(encodeAvgMs: Double, encodeMaxMs: Double) {
        active?.sendStats(encodeAvgMs, encodeMaxMs)
    }

    override fun onError(message: String) {
        Log.w(TAG, message)
        active?.sendCtrl(ctrl("error", "message" to message))
    }

    // ---- sessions ----

    private fun activate(session: Session) {
        val previous = synchronized(lock) { active.also { active = session } }
        if (previous != null && previous !== session) {
            previous.sendCtrl(ctrl("bye"))
            previous.close()
        }
        AppState.setStatus(Status.Connected(session.pcName))
    }

    private fun deactivate(session: Session) {
        val wasActive = synchronized(lock) { (active === session).also { if (it) active = null } }
        if (wasActive) {
            encoder.stop()
            host.onStreamingChanged(false)
            AppState.setStatus(Status.Waiting)
        }
    }

    private fun defaultCameraId(): String {
        val manager = context.getSystemService(CameraManager::class.java)
        return manager.cameraIdList.firstOrNull {
            manager.getCameraCharacteristics(it).get(CameraCharacteristics.LENS_FACING) == CameraCharacteristics.LENS_FACING_BACK
        } ?: manager.cameraIdList.first()
    }

    private sealed class Out {
        class Ctrl(val json: JSONObject) : Out()
        class Video(val ptsUs: Long, val flags: Int, val data: ByteArray) : Out()
        data object Stop : Out()
    }

    private inner class Session(private val socket: Socket) {
        private val queue = LinkedBlockingQueue<Out>()
        private val queuedVideo = AtomicInteger(0)
        @Volatile private var waitKeyframe = true
        @Volatile private var closed = false
        var pcName = "PC"
            private set

        fun run() {
            val address = socket.inetAddress
            try {
                socket.tcpNoDelay = true
                socket.soTimeout = READ_TIMEOUT_MS
                val reader = FrameReader(socket.getInputStream())
                val writer = FrameWriter(socket.getOutputStream())

                val version = Protocol.readHandshake(reader.input)
                Protocol.writeHandshake(writer.out)
                if (version != Protocol.VERSION) throw IOException("unsupported protocol version $version")

                val hello = (reader.read() as? Frame.Ctrl)?.json
                if (hello?.optString("type") != "hello") throw IOException("expected hello")
                val pcId = hello.optString("pc_id")
                pcName = hello.optString("pc_name", "PC").ifBlank { "PC" }

                // Writer thread drains the queue so slow sockets never block the encoder.
                thread(name = "writer-$pcName", isDaemon = true) { writeLoop(writer) }

                if (!authorize(pcId, address.isLoopbackAddress, address.hostAddress ?: "?")) {
                    sendCtrl(ctrl("denied", "reason" to "not allowed on the phone"))
                    queue.put(Out.Stop)
                    Thread.sleep(200)
                    return
                }

                sendCtrl(ctrl("caps",
                    "device_name" to Build.MODEL, "device_id" to trust.deviceId,
                    "cameras" to CameraEncoder.describeCameras(context)))
                activate(this)

                while (!closed) {
                    val frame = reader.read()
                    if (frame is Frame.Ctrl) handleCtrl(frame.json)
                }
            } catch (e: Exception) {
                if (!closed) Log.i(TAG, "session ended: $e")
            } finally {
                close()
                deactivate(this)
            }
        }

        private fun authorize(pcId: String, loopback: Boolean, address: String): Boolean {
            if (loopback) return true // adb forward over USB
            if (pcId.isNotBlank() && trust.isTrusted(pcId)) return true
            if (pcId.isBlank()) return false

            val future = CompletableFuture<Boolean>()
            synchronized(lock) {
                if (pending != null) return false // one prompt at a time
                pending = future
            }
            val request = PairingRequest(pcId, pcName, address)
            AppState.setStatus(Status.AwaitingApproval(pcName))
            host.showPairingPrompt(request)
            try {
                repeat(PAIRING_TIMEOUT_S / 5) {
                    sendCtrl(ctrl("pairing", "state" to "pending"))
                    try {
                        val allowed = future.get(5, TimeUnit.SECONDS)
                        if (allowed) trust.add(pcId, pcName)
                        return allowed
                    } catch (_: TimeoutException) {
                    }
                    if (closed) return false
                }
                return false
            } finally {
                synchronized(lock) { pending = null }
                host.hidePairingPrompt()
                if (active == null) AppState.setStatus(Status.Waiting)
            }
        }

        private fun handleCtrl(msg: JSONObject) {
            when (msg.optString("type")) {
                "config" -> {
                    waitKeyframe = true
                    val camera = msg.optString("camera").ifBlank { defaultCameraId() }
                    encoder.start(StreamConfig(
                        cameraId = camera,
                        width = msg.optInt("w", 1280),
                        height = msg.optInt("h", 720),
                        fps = msg.optInt("fps", 30),
                        bitrate = msg.optInt("bitrate", 6_000_000),
                    ))
                }
                "torch" -> encoder.setTorch(msg.optBoolean("on"))
                "bitrate" -> encoder.setBitrate(msg.optInt("bps", 6_000_000))
                "request_keyframe" -> encoder.requestKeyframe()
                "ping" -> sendCtrl(ctrl("pong", "t" to msg.opt("t")))
                "bye" -> close()
            }
        }

        fun sendStats(encodeAvgMs: Double, encodeMaxMs: Double) {
            sendCtrl(ctrl("stats", "encode_ms" to encodeAvgMs, "encode_max_ms" to encodeMaxMs, "queued" to queuedVideo.get()))
        }

        fun sendCtrl(json: JSONObject) {
            if (!closed) queue.put(Out.Ctrl(json))
        }

        fun sendVideo(ptsUs: Long, flags: Int, data: ByteArray) {
            if (closed) return
            if (flags and Protocol.FLAG_CONFIG != 0) {
                queue.put(Out.Video(ptsUs, flags, data))
                return
            }
            val key = flags and Protocol.FLAG_KEYFRAME != 0
            if (waitKeyframe && !key) return
            if (queuedVideo.get() >= MAX_QUEUED_VIDEO) {
                // Socket is backing up: skip ahead to the next IDR rather than building latency.
                if (!waitKeyframe) {
                    waitKeyframe = true
                    encoder.requestKeyframe()
                }
                return
            }
            waitKeyframe = false
            queuedVideo.incrementAndGet()
            queue.put(Out.Video(ptsUs, flags, data))
        }

        private fun writeLoop(writer: FrameWriter) {
            try {
                while (true) {
                    when (val item = queue.take()) {
                        is Out.Ctrl -> writer.writeCtrl(item.json)
                        is Out.Video -> {
                            writer.writeVideo(item.ptsUs, item.flags, item.data)
                            if (item.flags and Protocol.FLAG_CONFIG == 0) queuedVideo.decrementAndGet()
                        }
                        Out.Stop -> break
                    }
                }
            } catch (e: Exception) {
                if (!closed) Log.i(TAG, "write failed: $e")
            } finally {
                close()
            }
        }

        fun close() {
            if (closed) return
            closed = true
            queue.clear()
            queue.offer(Out.Stop)
            runCatching { socket.close() }
        }
    }
}
