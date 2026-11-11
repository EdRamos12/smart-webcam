package com.edu.smartwebcam

import android.annotation.SuppressLint
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

private const val TAG = "StreamService"
private const val CHANNEL_SERVICE = "service"
private const val CHANNEL_PAIRING = "pairing"
private const val NOTIF_SERVICE = 1
private const val NOTIF_PAIRING = 2

/**
 * Camera foreground service. It must be started while the activity is visible (Android 14+);
 * afterwards it can open the camera whenever a PC connects, even with the screen off.
 */
class StreamService : Service(), ServerHost {
    private lateinit var trust: TrustStore
    private var server: StreamServer? = null
    private var nsd: NsdAdvertiser? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        trust = TrustStore(this)
        trust.publish()
        createChannels()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_STOP -> {
                stopSelf()
                return START_NOT_STICKY
            }
            ACTION_ALLOW -> server?.resolvePairing(true)
            ACTION_DENY -> server?.resolvePairing(false)
            else -> if (server == null) startServing()
        }
        if (server == null) stopSelf()
        return START_NOT_STICKY
    }

    private fun startServing() {
        try {
            ServiceCompat.startForeground(this, NOTIF_SERVICE, serviceNotification(Status.Waiting),
                if (Build.VERSION.SDK_INT >= 30) ServiceInfo.FOREGROUND_SERVICE_TYPE_CAMERA else 0)
        } catch (e: Exception) {
            Log.e(TAG, "cannot start foreground service", e)
            stopSelf()
            return
        }
        val s = StreamServer(this, trust, this)
        try {
            s.start()
        } catch (e: Exception) {
            Log.e(TAG, "cannot bind port ${Protocol.PORT}", e)
            stopSelf()
            return
        }
        server = s
        nsd = NsdAdvertiser(this, Build.MODEL, trust.deviceId).also { it.start() }

        // Keep the ongoing notification in sync with the status.
        scope.launch {
            AppState.status.collect { status ->
                if (status != Status.Stopped) {
                    getSystemService(NotificationManager::class.java).notify(NOTIF_SERVICE, serviceNotification(status))
                }
            }
        }
    }

    override fun onDestroy() {
        scope.cancel()
        nsd?.stop()
        server?.stop()
        server = null
        releaseLocks()
        AppState.setPairing(null)
        AppState.setStatus(Status.Stopped)
        super.onDestroy()
    }

    // ---- ServerHost ----

    override fun showPairingPrompt(request: PairingRequest) {
        AppState.setPairing(request)
        val allow = PendingIntent.getService(this, 1, intent(this, ACTION_ALLOW), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val deny = PendingIntent.getService(this, 2, intent(this, ACTION_DENY), PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT)
        val notif = NotificationCompat.Builder(this, CHANNEL_PAIRING)
            .setSmallIcon(R.drawable.ic_stat_camera)
            .setContentTitle("Allow ${request.pcName} to use the camera?")
            .setContentText("Connection from ${request.address} over Wi-Fi")
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setCategory(NotificationCompat.CATEGORY_CALL)
            .setContentIntent(openAppIntent())
            .setAutoCancel(true)
            .addAction(0, "Allow", allow)
            .addAction(0, "Deny", deny)
            .build()
        getSystemService(NotificationManager::class.java).notify(NOTIF_PAIRING, notif)
    }

    override fun hidePairingPrompt() {
        AppState.setPairing(null)
        getSystemService(NotificationManager::class.java).cancel(NOTIF_PAIRING)
    }

    @SuppressLint("WakelockTimeout")
    override fun onStreamingChanged(streaming: Boolean) {
        if (!streaming) {
            releaseLocks()
            return
        }
        if (wakeLock == null) {
            wakeLock = getSystemService(PowerManager::class.java)
                .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "SmartWebcam:stream").apply { acquire() }
        }
        if (wifiLock == null) {
            val mode = if (Build.VERSION.SDK_INT >= 29) WifiManager.WIFI_MODE_FULL_LOW_LATENCY
            else @Suppress("DEPRECATION") WifiManager.WIFI_MODE_FULL_HIGH_PERF
            wifiLock = getSystemService(WifiManager::class.java).createWifiLock(mode, "SmartWebcam:stream").apply { acquire() }
        }
    }

    private fun releaseLocks() {
        wakeLock?.let { if (it.isHeld) it.release() }
        wifiLock?.let { if (it.isHeld) it.release() }
        wakeLock = null
        wifiLock = null
    }

    // ---- notifications ----

    private fun createChannels() {
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL_SERVICE, "Webcam service", NotificationManager.IMPORTANCE_LOW))
        nm.createNotificationChannel(NotificationChannel(CHANNEL_PAIRING, "Pairing requests", NotificationManager.IMPORTANCE_HIGH))
    }

    private fun serviceNotification(status: Status): Notification {
        val text = when (status) {
            is Status.Streaming -> "Streaming ${status.height}p${status.fps} to ${status.pcName}"
            is Status.Connected -> "Connected to ${status.pcName}"
            is Status.AwaitingApproval -> "Waiting for approval of ${status.pcName}"
            else -> "Waiting for a PC on port ${Protocol.PORT}"
        }
        val stop = PendingIntent.getService(this, 3, intent(this, ACTION_STOP), PendingIntent.FLAG_IMMUTABLE)
        return NotificationCompat.Builder(this, CHANNEL_SERVICE)
            .setSmallIcon(R.drawable.ic_stat_camera)
            .setContentTitle("Smart Webcam")
            .setContentText(text)
            .setOngoing(true)
            .setOnlyAlertOnce(true)
            .setContentIntent(openAppIntent())
            .addAction(0, "Stop", stop)
            .build()
    }

    private fun openAppIntent() = PendingIntent.getActivity(this, 0,
        Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE)

    companion object {
        const val ACTION_START = "com.edu.smartwebcam.START"
        const val ACTION_STOP = "com.edu.smartwebcam.STOP"
        const val ACTION_ALLOW = "com.edu.smartwebcam.ALLOW"
        const val ACTION_DENY = "com.edu.smartwebcam.DENY"

        fun intent(context: Context, action: String) = Intent(context, StreamService::class.java).setAction(action)

        fun start(context: Context) = ContextCompat.startForegroundService(context, intent(context, ACTION_START))

        fun send(context: Context, action: String) = context.startService(intent(context, action))
    }
}
