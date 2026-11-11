package com.edu.smartwebcam

import org.json.JSONObject
import java.io.BufferedInputStream
import java.io.BufferedOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream

/** Wire protocol, mirrors protocol/PROTOCOL.md and desktop/src/protocol.rs. */
object Protocol {
    const val PORT = 47800
    const val VERSION = 1
    const val SERVICE_TYPE = "_smartwebcam._tcp"
    val MAGIC = "SWCM".toByteArray(Charsets.US_ASCII)

    const val TYPE_CTRL = 1
    const val TYPE_VIDEO = 2

    const val FLAG_KEYFRAME = 1
    const val FLAG_CONFIG = 2

    const val MAX_FRAME = 16 shl 20

    /** Reads the peer's handshake and returns its version. */
    fun readHandshake(input: DataInputStream): Int {
        val magic = ByteArray(4)
        input.readFully(magic)
        if (!magic.contentEquals(MAGIC)) throw IOException("bad magic")
        return input.readUnsignedShort()
    }

    fun writeHandshake(out: DataOutputStream) {
        out.write(MAGIC)
        out.writeShort(VERSION)
        out.flush()
    }
}

sealed class Frame {
    data class Ctrl(val json: JSONObject) : Frame()
    class Video(val ptsUs: Long, val flags: Int, val data: ByteArray) : Frame()
    class Unknown(val type: Int) : Frame()
}

class FrameReader(stream: InputStream) {
    val input = DataInputStream(BufferedInputStream(stream, 16 * 1024))

    fun read(): Frame {
        val type = input.readUnsignedByte()
        val len = input.readInt()
        if (len < 0 || len > Protocol.MAX_FRAME) throw IOException("frame too large: $len")
        val payload = ByteArray(len)
        input.readFully(payload)
        return when (type) {
            Protocol.TYPE_CTRL -> Frame.Ctrl(JSONObject(String(payload, Charsets.UTF_8)))
            Protocol.TYPE_VIDEO -> {
                if (len < 9) throw IOException("short video frame")
                val pts = java.nio.ByteBuffer.wrap(payload, 0, 8).long
                Frame.Video(pts, payload[8].toInt() and 0xff, payload.copyOfRange(9, len))
            }
            else -> Frame.Unknown(type)
        }
    }
}

class FrameWriter(stream: OutputStream) {
    val out = DataOutputStream(BufferedOutputStream(stream, 64 * 1024))

    @Synchronized
    fun writeCtrl(json: JSONObject) {
        val bytes = json.toString().toByteArray(Charsets.UTF_8)
        out.writeByte(Protocol.TYPE_CTRL)
        out.writeInt(bytes.size)
        out.write(bytes)
        out.flush()
    }

    @Synchronized
    fun writeVideo(ptsUs: Long, flags: Int, data: ByteArray) {
        out.writeByte(Protocol.TYPE_VIDEO)
        out.writeInt(9 + data.size)
        out.writeLong(ptsUs)
        out.writeByte(flags)
        out.write(data)
        out.flush()
    }
}

fun ctrl(type: String, vararg fields: Pair<String, Any?>): JSONObject =
    JSONObject().apply {
        put("type", type)
        for ((k, v) in fields) put(k, v)
    }
