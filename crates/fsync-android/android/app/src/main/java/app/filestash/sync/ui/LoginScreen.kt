package app.filestash.sync.ui

import android.net.Uri
import android.webkit.CookieManager
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import app.filestash.sync.Native

@Composable
fun LoginScreen(onLoggedIn: () -> Unit) {
    val context = LocalContext.current
    var url by remember { mutableStateOf("") }
    var server by remember { mutableStateOf<String?>(null) }

    server?.let { base ->
        BackHandler { server = null }
        LoginWebView(
            base = base,
            onToken = { token ->
                Native.session(context, base, token)
                onLoggedIn()
            },
        )
        return
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp, Alignment.CenterVertically),
        horizontalAlignment = Alignment.CenterHorizontally,
    ) {
        Text("Filestash", style = MaterialTheme.typography.headlineLarge)
        Text(
            "Connect to your server",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        OutlinedTextField(
            value = url, onValueChange = { url = it },
            label = { Text("Server") }, singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        Button(
            onClick = {
                val base = if ("://" in url) url else "https://$url"
                server = base.trimEnd('/')
            },
            enabled = url.isNotBlank(),
            shape = MaterialTheme.shapes.extraSmall,
            modifier = Modifier
                .fillMaxWidth()
                .height(56.dp),
        ) {
            Text("Connect")
        }
    }
}

@Composable
private fun LoginWebView(base: String, onToken: (String) -> Unit) {
    var done by remember { mutableStateOf(false) }
    AndroidView(
        factory = { ctx ->
            WebView(ctx).apply {
                settings.javaScriptEnabled = true
                settings.domStorageEnabled = true
                webViewClient = object : WebViewClient() {
                    override fun doUpdateVisitedHistory(view: WebView?, url: String?, isReload: Boolean) {
                        android.util.Log.i("fsync", "login visited: $url")
                        if (done || url == null) return
                        val path = Uri.parse(url).path ?: return
                        if (path.startsWith("/files")) {
                            val token = sessionToken(base)
                            android.util.Log.i(
                                "fsync",
                                "login harvest: cookies=${CookieManager.getInstance().getCookie("$base/api/") != null} token=${token?.length ?: 0}",
                            )
                            token?.let {
                                done = true
                                onToken(it)
                            }
                        }
                    }
                }
                loadUrl("$base/login")
            }
        },
        modifier = Modifier.fillMaxSize(),
    )
}

private fun sessionToken(base: String): String? {
    val cookies = CookieManager.getInstance().getCookie("$base/api/") ?: return null
    val auth = Regex("^auth(\\d*)$")
    val parts = cookies.split(';')
        .mapNotNull { cookie ->
            val (name, value) = cookie.trim().split('=', limit = 2)
                .takeIf { it.size == 2 } ?: return@mapNotNull null
            val order = auth.find(name)?.groupValues?.get(1) ?: return@mapNotNull null
            (order.toIntOrNull() ?: 0) to value
        }
        .sortedBy { it.first }
        .map { it.second }
    return parts.joinToString("").ifEmpty { null }
}
