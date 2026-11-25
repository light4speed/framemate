package dev.framemate.app

import android.webkit.JavascriptInterface
import androidx.core.content.ContextCompat
import androidx.mediarouter.media.MediaRouteSelector
import androidx.mediarouter.media.MediaRouter
import com.google.android.gms.cast.CastMediaControlIntent
import com.google.android.gms.cast.MediaInfo
import com.google.android.gms.cast.MediaLoadRequestData
import com.google.android.gms.cast.MediaMetadata
import com.google.android.gms.cast.MediaStatus
import com.google.android.gms.cast.framework.CastContext
import com.google.android.gms.cast.framework.CastSession
import com.google.android.gms.cast.framework.SessionManagerListener
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.CopyOnWriteArrayList

/**
 * `window.FrameMateCast`: the Google Cast SDK, which a WebView can't use itself.
 *
 * The stream is produced and served by Rust (`cast_start`); this only discovers receivers,
 * starts a session and tells it which URL to play. JS polls the synchronous getters, the same
 * pattern as the notification bridge.
 */
class CastBridge(private val activity: MainActivity) {
    private val router = MediaRouter.getInstance(activity)
    private val selector = MediaRouteSelector.Builder()
        .addControlCategory(
            CastMediaControlIntent.categoryForCast("6DAD82C5"),
        )
        .build()

    /** Kept up to date by [discover]'s callbacks; read from the WebView thread. */
    private val routes = CopyOnWriteArrayList<MediaRouter.RouteInfo>()

    @Volatile private var available = false
    @Volatile private var error: String? = null
    @Volatile private var castContext: CastContext? = null
    private var callback: MediaRouter.Callback? = null
    private var listener: SessionManagerListener<CastSession>? = null
    private var pending: PendingLoad? = null
    private var lastRouteId: String? = null

    private data class PendingLoad(val url: String, val title: String)

    /** Called from the activity once it exists; `CastContext` initialises asynchronously. */
    fun initialize() {
        if (castContext != null) return
        CastContext.getSharedInstance(activity, ContextCompat.getMainExecutor(activity))
            .addOnSuccessListener { context ->
                castContext = context
                available = true
                val manager = context.sessionManager
                val sessionListener = object : SessionManagerListener<CastSession> {
                    override fun onSessionStarting(session: CastSession) {}

                    override fun onSessionStarted(session: CastSession, sessionId: String) = load()

                    override fun onSessionStartFailed(session: CastSession, error: Int) {
                        this@CastBridge.error = "Couldn't connect to the TV (error $error)."
                        pending = null
                    }

                    override fun onSessionEnding(session: CastSession) {}

                    override fun onSessionEnded(session: CastSession, error: Int) {
                        // A pending load survives: switching TVs ends the old session first.
                    }

                    override fun onSessionResuming(session: CastSession, sessionId: String) {}

                    override fun onSessionResumed(session: CastSession, wasSuspended: Boolean) = load()

                    override fun onSessionResumeFailed(session: CastSession, error: Int) {}

                    override fun onSessionSuspended(session: CastSession, reason: Int) {}
                }
                listener = sessionListener
                manager.addSessionManagerListener(sessionListener, CastSession::class.java)
                discover()
            }
            .addOnFailureListener { failure ->
                available = false
                error = "Casting needs Google Play Services: ${failure.message ?: "not available"}"
            }
    }

    /** Stops discovery; the session itself keeps playing while the activity is recreated. */
    fun release() {
        callback?.let { router.removeCallback(it) }
        callback = null
        listener?.let { castContext?.sessionManager?.removeSessionManagerListener(it, CastSession::class.java) }
        listener = null
    }

    private fun discover() {
        if (callback != null) return
        val discovering = object : MediaRouter.Callback() {
            override fun onRouteAdded(router: MediaRouter, route: MediaRouter.RouteInfo) = add(route)

            override fun onRouteRemoved(router: MediaRouter, route: MediaRouter.RouteInfo) {
                routes.remove(route)
            }

            override fun onRouteChanged(router: MediaRouter, route: MediaRouter.RouteInfo) {
                if (!routes.contains(route)) add(route)
            }
        }
        callback = discovering
        router.addCallback(selector, discovering, MediaRouter.CALLBACK_FLAG_REQUEST_DISCOVERY)
        // Callbacks only report later changes; seed the list with what is already known.
        for (route in router.routes) add(route)
    }

    private fun add(route: MediaRouter.RouteInfo) {
        if (route.isDefault || !route.matchesSelector(selector)) return
        if (!routes.contains(route)) routes.add(route)
    }

    @JavascriptInterface
    fun castAvailable(): Boolean = available

    /** `[{id, name, description}]` */
    @JavascriptInterface
    fun castDevices(): String {
        val devices = JSONArray()
        for (route in routes) {
            devices.put(
                JSONObject()
                    .put("id", route.id)
                    .put("name", route.name)
                    .put("description", route.description ?: ""),
            )
        }
        return devices.toString()
    }

    /** `{available, connected, device, playing, error}` */
    @JavascriptInterface
    fun castStatus(): String {
        val session = castContext?.sessionManager?.currentCastSession
        val connected = session?.isConnected == true
        return JSONObject()
            .put("available", available)
            .put("connected", connected)
            .put("device", if (connected) session?.castDevice?.friendlyName else JSONObject.NULL)
            .put(
                "playing",
                session?.remoteMediaClient?.mediaStatus?.playerState == MediaStatus.PLAYER_STATE_PLAYING,
            )
            .put("error", error ?: JSONObject.NULL)
            .toString()
    }

    @JavascriptInterface
    fun castPlay(routeId: String, url: String, title: String) {
        activity.runOnUiThread {
            error = null
            pending = PendingLoad(url, title)
            val session = castContext?.sessionManager?.currentCastSession
            if (session?.isConnected == true && lastRouteId == routeId) {
                load()
                return@runOnUiThread
            }
            val route = routes.firstOrNull { it.id == routeId }
            if (route == null) {
                error = "That TV isn't available any more. Pick it again."
                pending = null
                return@runOnUiThread
            }
            lastRouteId = routeId
            route.select()
        }
    }

    @JavascriptInterface
    fun castStop() {
        activity.runOnUiThread {
            pending = null
            lastRouteId = null
            error = null
            castContext?.sessionManager?.endCurrentSession(true)
        }
    }

    /** Called once the session exists, and again if it is resumed. */
    private fun load() {
        val pending = pending ?: return
        val client = castContext?.sessionManager?.currentCastSession?.remoteMediaClient ?: return
        val metadata = MediaMetadata(MediaMetadata.MEDIA_TYPE_MOVIE).apply {
            putString(MediaMetadata.KEY_TITLE, pending.title)
        }
        val info = MediaInfo.Builder(pending.url)
            .setContentUrl(pending.url)
            .setStreamType(MediaInfo.STREAM_TYPE_LIVE)
            .setContentType("application/vnd.apple.mpegurl")
            .setMetadata(metadata)
            .build()
        // Nudge the receiver toward the live edge: a very large time clamps to the end of
        // the available buffer instead of starting several segments back.
        client.load(MediaLoadRequestData.Builder().setMediaInfo(info).setAutoplay(true).build())
        this.pending = null
    }
}
