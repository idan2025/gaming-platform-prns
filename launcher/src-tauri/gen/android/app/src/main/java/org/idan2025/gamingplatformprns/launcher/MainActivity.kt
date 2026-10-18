package org.idan2025.gamingplatformprns.launcher

import android.content.Context
import android.net.wifi.WifiManager
import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  private var multicast: WifiManager.MulticastLock? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // Without this, Wi-Fi drops the multicast Reticulum's LAN interface finds
    // peers with, and a phone hears no server on its own network.
    val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
    multicast = wifi.createMulticastLock("lanthorn").apply {
      setReferenceCounted(false)
      acquire()
    }
    // Edge to edge puts the page under the status bar, the camera cutout and
    // the gesture bar. Pad the content by them (and by the keyboard) so the
    // page lays out in what is left.
    val content = findViewById<View>(android.R.id.content)
    ViewCompat.setOnApplyWindowInsetsListener(content) { view, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or
          WindowInsetsCompat.Type.displayCutout() or
          WindowInsetsCompat.Type.ime()
      )
      view.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }

  override fun onDestroy() {
    multicast?.release()
    super.onDestroy()
  }
}
