package app.filestash.sync

import android.content.Context
import android.provider.DocumentsContract
import android.webkit.CookieManager
import app.filestash.core.Adapter
import app.filestash.core.FsException
import app.filestash.core.login as coreLogin
import java.io.File

object Native {
    const val AUTHORITY = "app.filestash.sync.documents"
    const val ROOT_ID = "filestash"

    @Volatile
    var client: Adapter? = null
        private set
    private var store: CredentialStore? = null

    @Synchronized
    fun init(context: Context): CredentialStore {
        val store = store ?: CredentialStore(context.applicationContext).also { store = it }
        if (client == null) {
            store.load()?.let { creds ->
                client = Adapter(creds.url, creds.insecure, creds.token, dataDir(context))
            }
        }
        return store
    }

    @Synchronized
    fun login(context: Context, credentials: Credentials) {
        val store = init(context)
        val token = coreLogin(
            credentials.url, credentials.insecure,
            credentials.user, credentials.password, credentials.storage,
        )
        store.save(credentials.copy(token = token))
        client?.close()
        client = Adapter(credentials.url, credentials.insecure, token, dataDir(context))
        notifyRootsChanged(context)
    }

    @Synchronized
    fun session(context: Context, url: String, token: String) {
        val store = init(context)
        store.save(Credentials(url, false, "", "", "", token))
        client?.close()
        client = Adapter(url, false, token, dataDir(context))
        notifyRootsChanged(context)
    }

    @Synchronized
    fun logout(context: Context) {
        init(context).clear()
        CookieManager.getInstance().removeAllCookies(null)
        val old = client
        client = null
        notifyRootsChanged(context)
        if (old != null) {
            Thread {
                try {
                    old.flush(10_000u)
                    old.logout()
                    android.util.Log.i("fsync", "logout: session invalidated")
                } catch (e: Exception) {
                    android.util.Log.e("fsync", "logout", e)
                } finally {
                    old.close()
                }
            }.start()
        }
    }

    fun <T> withReauth(context: Context, block: (Adapter) -> T): T {
        val store = init(context)
        val client = client ?: throw FsException.NotAuthenticated()
        return try {
            block(client)
        } catch (e: FsException) {
            if (e !is FsException.NotAuthenticated && e !is FsException.PermissionDenied) throw e
            val creds = store.load() ?: throw e
            if (creds.password.isEmpty()) throw e
            relogin(context, creds)
            block(Native.client ?: throw e)
        }
    }

    @Synchronized
    private fun relogin(context: Context, creds: Credentials) {
        val token = coreLogin(creds.url, creds.insecure, creds.user, creds.password, creds.storage)
        store?.saveToken(token)
        client?.close()
        client = Adapter(creds.url, creds.insecure, token, dataDir(context))
    }

    private fun dataDir(context: Context): String =
        File(context.applicationContext.filesDir, "fsync").absolutePath

    fun notifyRootsChanged(context: Context) {
        context.contentResolver.notifyChange(DocumentsContract.buildRootsUri(AUTHORITY), null)
    }
}
