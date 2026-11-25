package dev.framemate.app

import android.content.Context
import com.google.android.gms.cast.framework.CastOptions
import com.google.android.gms.cast.framework.OptionsProvider
import com.google.android.gms.cast.framework.SessionProvider

/**
 * Tells the Cast SDK which receiver to use. Uses the custom low-latency receiver hosted at
 * https://light4speed.github.io/framemate/ (registered app ID 6DAD82C5).
 */
class CastOptionsProvider : OptionsProvider {
    override fun getCastOptions(context: Context): CastOptions =
        CastOptions.Builder()
            .setReceiverApplicationId("6DAD82C5")
            .build()

    override fun getAdditionalSessionProviders(context: Context): List<SessionProvider>? = null
}
