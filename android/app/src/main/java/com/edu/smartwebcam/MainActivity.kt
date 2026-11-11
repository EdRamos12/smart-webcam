package com.edu.smartwebcam

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import java.net.Inet4Address
import java.net.NetworkInterface

class MainActivity : ComponentActivity() {
    private lateinit var trust: TrustStore

    private val permissions = buildList {
        add(Manifest.permission.CAMERA)
        if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
    }.toTypedArray()

    private val requestPermissions = registerForActivityResult(ActivityResultContracts.RequestMultiplePermissions()) {
        if (hasCamera()) StreamService.start(this)
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        trust = TrustStore(this)
        trust.publish()

        setContent {
            val dark = isSystemInDarkTheme()
            MaterialTheme(colorScheme = if (dark) darkColorScheme() else lightColorScheme()) {
                Screen(
                    onStart = { startService() },
                    onStop = { StreamService.send(this, StreamService.ACTION_STOP) },
                    onForget = { trust.remove(it) },
                    onPairing = { allow ->
                        StreamService.send(this, if (allow) StreamService.ACTION_ALLOW else StreamService.ACTION_DENY)
                    },
                )
            }
        }

        // Start serving right away; the camera itself only opens when a PC connects.
        startService()
    }

    private fun hasCamera() =
        ContextCompat.checkSelfPermission(this, Manifest.permission.CAMERA) == PackageManager.PERMISSION_GRANTED

    private fun startService() {
        val missing = permissions.filter {
            ContextCompat.checkSelfPermission(this, it) != PackageManager.PERMISSION_GRANTED
        }
        if (missing.isEmpty()) StreamService.start(this) else requestPermissions.launch(missing.toTypedArray())
    }
}

private fun localAddresses(): List<String> = runCatching {
    NetworkInterface.getNetworkInterfaces().toList()
        .filter { it.isUp && !it.isLoopback }
        .flatMap { iface -> iface.inetAddresses.toList().filterIsInstance<Inet4Address>().map { "${it.hostAddress} (${iface.name})" } }
}.getOrDefault(emptyList())

@Composable
private fun Screen(
    onStart: () -> Unit,
    onStop: () -> Unit,
    onForget: (String) -> Unit,
    onPairing: (Boolean) -> Unit,
) {
    val status by AppState.status.collectAsState()
    val pairing by AppState.pairing.collectAsState()
    val trusted by AppState.trusted.collectAsState()

    Scaffold { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(20.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            Text("Smart Webcam", style = MaterialTheme.typography.headlineMedium, fontWeight = FontWeight.Bold)

            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text(statusText(status), style = MaterialTheme.typography.titleMedium)
                    if (status != Status.Stopped) {
                        Text("Port ${Protocol.PORT}", style = MaterialTheme.typography.bodyMedium)
                        localAddresses().forEach { Text(it, style = MaterialTheme.typography.bodySmall) }
                    }
                    Spacer(Modifier.padding(2.dp))
                    if (status == Status.Stopped) {
                        Button(onClick = onStart) { Text("Start") }
                    } else {
                        OutlinedButton(onClick = onStop) { Text("Stop service") }
                    }
                }
            }

            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("How to connect", style = MaterialTheme.typography.titleMedium)
                    Text("• USB: plug in the phone (USB debugging on) and pick it under Connect in the PC tray.",
                        style = MaterialTheme.typography.bodyMedium)
                    Text("• Wi-Fi: same network; the phone shows up automatically in the tray. The first time you'll be asked to allow the PC here.",
                        style = MaterialTheme.typography.bodyMedium)
                    Text("• You can lock the screen while streaming to save battery; the camera keeps running.",
                        style = MaterialTheme.typography.bodyMedium)
                }
            }

            Card(Modifier.fillMaxWidth()) {
                Column(Modifier.padding(16.dp), verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    Text("Trusted PCs (Wi-Fi)", style = MaterialTheme.typography.titleMedium)
                    if (trusted.isEmpty()) Text("None yet", style = MaterialTheme.typography.bodyMedium)
                    trusted.forEach { (id, name) ->
                        Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                            Text(name, Modifier.weight(1f))
                            Spacer(Modifier.width(8.dp))
                            TextButton(onClick = { onForget(id) }) { Text("Forget") }
                        }
                    }
                }
            }
        }
    }

    pairing?.let { req ->
        AlertDialog(
            onDismissRequest = {},
            title = { Text("Allow ${req.pcName}?") },
            text = { Text("${req.pcName} (${req.address}) wants to use this phone's camera over Wi-Fi.") },
            confirmButton = { TextButton(onClick = { onPairing(true) }) { Text("Allow") } },
            dismissButton = { TextButton(onClick = { onPairing(false) }) { Text("Deny") } },
        )
    }
}

private fun statusText(status: Status) = when (status) {
    Status.Stopped -> "Stopped"
    Status.Waiting -> "Waiting for a PC…"
    is Status.AwaitingApproval -> "${status.pcName} wants to connect"
    is Status.Connected -> "Connected to ${status.pcName}"
    is Status.Streaming -> "Streaming ${status.width}×${status.height} @ ${status.fps} fps (${status.facing}) to ${status.pcName}"
}
