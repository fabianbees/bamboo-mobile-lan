package com.joelsgc.bamboomobilelan

import android.net.Uri
import android.os.Bundle
import android.view.View
import android.view.WindowManager
import android.widget.TextView
import androidx.annotation.OptIn
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import androidx.media3.common.MediaItem
import androidx.media3.common.PlaybackException
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.DefaultLoadControl
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.rtsp.RtspMediaSource
import androidx.media3.ui.PlayerView

/** Full-screen RTSPS camera stream (X1/H2/X2 series), played with Media3 ExoPlayer. */
@OptIn(UnstableApi::class)
class LiveViewActivity : AppCompatActivity() {

    private lateinit var playerView: PlayerView
    private lateinit var status: TextView
    private var player: ExoPlayer? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_live_view)
        playerView = findViewById(R.id.player_view)
        status = findViewById(R.id.status)

        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    override fun onStart() {
        super.onStart()
        startPlayer()
    }

    override fun onStop() {
        super.onStop()
        playerView.player = null
        player?.release()
        player = null
    }

    private fun startPlayer() {
        val url = intent.getStringExtra(EXTRA_URL).orEmpty()
        val accessCode = intent.getStringExtra(EXTRA_ACCESS_CODE).orEmpty()
        val printerUri = Uri.parse(url)
        val uri = printerUri.buildUpon()
            .encodedAuthority("bblp:${Uri.encode(accessCode)}@${printerUri.encodedAuthority}")
            .build()

        val source = RtspMediaSource.Factory()
            .setForceUseRtpTcp(true)
            .setSocketFactory(LanTls.socketFactory())
            .createMediaSource(MediaItem.fromUri(uri))

        // Small buffers: a live camera should start fast and stay close to real time.
        val loadControl = DefaultLoadControl.Builder()
            .setBufferDurationsMs(MIN_BUFFER_MS, MAX_BUFFER_MS, START_BUFFER_MS, REBUFFER_MS)
            .build()

        player = ExoPlayer.Builder(this).setLoadControl(loadControl).build().also { p ->
            playerView.player = p
            p.addListener(object : Player.Listener {
                override fun onPlaybackStateChanged(state: Int) {
                    if (state == Player.STATE_READY) {
                        status.visibility = View.GONE
                    } else if (state == Player.STATE_BUFFERING) {
                        status.text = getString(R.string.live_view_connecting)
                        status.visibility = View.VISIBLE
                    }
                }

                override fun onPlayerError(error: PlaybackException) {
                    status.text = getString(
                        R.string.live_view_failed,
                        error.errorCodeName,
                        error.cause?.message.orEmpty(),
                    )
                    status.visibility = View.VISIBLE
                }
            })
            p.setMediaSource(source)
            p.playWhenReady = true
            p.prepare()
        }
    }

    companion object {
        const val EXTRA_URL = "url"
        const val EXTRA_ACCESS_CODE = "access_code"

        private const val MIN_BUFFER_MS = 1000
        private const val MAX_BUFFER_MS = 3000
        private const val START_BUFFER_MS = 250
        private const val REBUFFER_MS = 500
    }
}
