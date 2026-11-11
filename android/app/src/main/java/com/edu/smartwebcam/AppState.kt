package com.edu.smartwebcam

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import org.json.JSONObject
import java.util.UUID

sealed class Status {
    data object Stopped : Status()
    data object Waiting : Status()
    data class AwaitingApproval(val pcName: String) : Status()
    data class Connected(val pcName: String) : Status()
    data class Streaming(val pcName: String, val width: Int, val height: Int, val fps: Int, val facing: String) : Status()
}

data class PairingRequest(val pcId: String, val pcName: String, val address: String)

/** Process-wide observable state shared by the service and the UI. */
object AppState {
    private val _status = MutableStateFlow<Status>(Status.Stopped)
    val status: StateFlow<Status> = _status.asStateFlow()

    private val _pairing = MutableStateFlow<PairingRequest?>(null)
    val pairing: StateFlow<PairingRequest?> = _pairing.asStateFlow()

    private val _trusted = MutableStateFlow<Map<String, String>>(emptyMap())
    val trusted: StateFlow<Map<String, String>> = _trusted.asStateFlow()

    fun setStatus(s: Status) { _status.value = s }
    fun setPairing(p: PairingRequest?) { _pairing.value = p }
    fun setTrusted(t: Map<String, String>) { _trusted.value = t }
}

/** Persists the device id and the PCs the user has allowed. */
class TrustStore(context: Context) {
    private val prefs = context.getSharedPreferences("smartwebcam", Context.MODE_PRIVATE)

    val deviceId: String
        get() = prefs.getString("device_id", null) ?: UUID.randomUUID().toString().also {
            prefs.edit().putString("device_id", it).apply()
        }

    fun all(): Map<String, String> {
        val json = JSONObject(prefs.getString("trusted", "{}")!!)
        return json.keys().asSequence().associateWith { json.getString(it) }
    }

    fun isTrusted(pcId: String) = all().containsKey(pcId)

    fun add(pcId: String, pcName: String) = save(all() + (pcId to pcName))

    fun remove(pcId: String) = save(all() - pcId)

    fun publish() = AppState.setTrusted(all())

    private fun save(map: Map<String, String>) {
        prefs.edit().putString("trusted", JSONObject(map).toString()).apply()
        AppState.setTrusted(map)
    }
}
