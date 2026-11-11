package com.edu.smartwebcam

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.util.Log

private const val TAG = "NsdAdvertiser"

/** Advertises `_smartwebcam._tcp` with TXT `name`, `id`, `v` so PCs on the LAN can find the phone. */
class NsdAdvertiser(context: Context, private val name: String, private val id: String) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private var listener: NsdManager.RegistrationListener? = null

    fun start() {
        if (listener != null) return
        val info = NsdServiceInfo().apply {
            serviceName = "Smart Webcam ($name)"
            serviceType = Protocol.SERVICE_TYPE
            port = Protocol.PORT
            setAttribute("name", name)
            setAttribute("id", id)
            setAttribute("v", Protocol.VERSION.toString())
        }
        val l = object : NsdManager.RegistrationListener {
            override fun onServiceRegistered(info: NsdServiceInfo) = Log.i(TAG, "registered ${info.serviceName}").let { }
            override fun onRegistrationFailed(info: NsdServiceInfo, error: Int) = Log.w(TAG, "registration failed: $error").let { }
            override fun onServiceUnregistered(info: NsdServiceInfo) {}
            override fun onUnregistrationFailed(info: NsdServiceInfo, error: Int) {}
        }
        listener = l
        nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, l)
    }

    fun stop() {
        listener?.let { runCatching { nsd.unregisterService(it) } }
        listener = null
    }
}
