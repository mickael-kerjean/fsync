package app.filestash.sync.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.provider.DocumentsContract
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.Path
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import app.filestash.sync.Native
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@Composable
fun HomeScreen(onLoggedOut: () -> Unit) {
    val context = LocalContext.current
    var connected by remember { mutableStateOf<Boolean?>(null) }
    var checkNow by remember { mutableIntStateOf(0) }
    val account = remember { accountLabel(context) }

    LaunchedEffect(checkNow) {
        while (true) {
            connected = withContext(Dispatchers.IO) {
                runCatching { Native.withReauth(context) { it.ls("/") } }.isSuccess
            }
            delay(10_000)
        }
    }

    Box(modifier = Modifier.fillMaxSize()) {
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(24.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            Spacer(modifier = Modifier.weight(1f))
            StatusCircle(connected) { checkNow++ }
            Spacer(modifier = Modifier.height(16.dp))
            Text(
                when (connected) {
                    true -> "Connected"
                    false -> "Offline"
                    null -> "Connecting…"
                },
                style = MaterialTheme.typography.titleMedium,
            )
            account?.let {
                Spacer(modifier = Modifier.height(4.dp))
                Text(it, style = MaterialTheme.typography.bodySmall, color = MaterialTheme.colorScheme.outline)
            }
            Spacer(modifier = Modifier.weight(1f))
            Button(
                onClick = { openFileManager(context) },
                colors = ButtonDefaults.buttonColors(
                    containerColor = MaterialTheme.colorScheme.secondary,
                    contentColor = MaterialTheme.colorScheme.onSecondary,
                ),
                shape = MaterialTheme.shapes.extraSmall,
                modifier = Modifier
                    .fillMaxWidth()
                    .height(56.dp),
            ) {
                Text("Browse")
            }
        }
        IconButton(
            onClick = {
                CoroutineScope(Dispatchers.IO).launch { Native.logout(context) }
                onLoggedOut()
            },
            modifier = Modifier.align(Alignment.TopEnd),
        ) {
            LogoutIcon(MaterialTheme.colorScheme.onSurfaceVariant)
        }
    }
}

@Composable
private fun LogoutIcon(tint: Color) {
    Canvas(modifier = Modifier.size(22.dp)) {
        val w = size.width
        val h = size.height
        val stroke = Stroke(width = w / 11f, cap = StrokeCap.Round, join = StrokeJoin.Round)
        // Door frame, open on the right.
        drawPath(
            Path().apply {
                moveTo(w * 0.52f, h * 0.15f)
                lineTo(w * 0.17f, h * 0.15f)
                lineTo(w * 0.17f, h * 0.85f)
                lineTo(w * 0.52f, h * 0.85f)
            },
            color = tint,
            style = stroke,
        )
        // Arrow leaving through the opening.
        drawLine(
            color = tint,
            start = Offset(w * 0.40f, h * 0.5f),
            end = Offset(w * 0.88f, h * 0.5f),
            strokeWidth = stroke.width,
            cap = StrokeCap.Round,
        )
        drawPath(
            Path().apply {
                moveTo(w * 0.70f, h * 0.31f)
                lineTo(w * 0.90f, h * 0.5f)
                lineTo(w * 0.70f, h * 0.69f)
            },
            color = tint,
            style = stroke,
        )
    }
}

@Composable
private fun StatusCircle(connected: Boolean?, onClick: () -> Unit) {
    val fill = when (connected) {
        true -> ConnectedGreen
        false -> MaterialTheme.colorScheme.error
        null -> MaterialTheme.colorScheme.surfaceVariant
    }
    val glyph = when (connected) {
        null -> MaterialTheme.colorScheme.outline
        else -> Color(0xFF24272A)
    }
    val pulse by rememberInfiniteTransition(label = "glow").animateFloat(
        initialValue = 0f,
        targetValue = 1f,
        animationSpec = infiniteRepeatable(
            animation = tween(1600, easing = FastOutSlowInEasing),
            repeatMode = RepeatMode.Reverse,
        ),
        label = "glow",
    )
    val glow = if (connected == true) pulse else 0f
    Box(
        modifier = Modifier
            .size(168.dp)
            .clip(CircleShape)
            .background(fill.copy(alpha = 0.14f + 0.10f * glow)),
        contentAlignment = Alignment.Center,
    ) {
        Box(
            modifier = Modifier
                .size(146.dp)
                .clip(CircleShape)
                .background(fill.copy(alpha = 0.22f + 0.12f * glow)),
        )
        StatusCircleCore(fill, glyph, onClick)
    }
}

@Composable
private fun StatusCircleCore(fill: Color, glyph: Color, onClick: () -> Unit) {
    Box(
        modifier = Modifier
            .size(124.dp)
            .clip(CircleShape)
            .background(fill)
            .clickable(onClick = onClick),
        contentAlignment = Alignment.Center,
    ) {
        Canvas(modifier = Modifier.size(48.dp)) {
            val stroke = Stroke(width = size.width / 9, cap = StrokeCap.Round)
            drawArc(
                color = glyph,
                startAngle = -60f,
                sweepAngle = 300f,
                useCenter = false,
                style = stroke,
            )
            drawLine(
                color = glyph,
                start = Offset(size.width / 2, -size.height / 12),
                end = Offset(size.width / 2, size.height / 2.4f),
                strokeWidth = stroke.width,
                cap = StrokeCap.Round,
            )
        }
    }
}

private fun accountLabel(context: Context): String? {
    val creds = Native.init(context).load() ?: return null
    val host = creds.url.substringAfter("://").substringBefore('/')
    val who = creds.user.ifEmpty { null }
    return listOfNotNull(who, "$host/${creds.storage}".trimEnd('/')).joinToString(" @ ")
}

private fun openFileManager(context: Context) {
    val rootUri = DocumentsContract.buildRootUri(Native.AUTHORITY, Native.ROOT_ID)
    val browse = Intent(Intent.ACTION_VIEW)
        .setDataAndType(rootUri, DocumentsContract.Root.MIME_TYPE_ITEM)
    try {
        context.startActivity(browse)
    } catch (e: ActivityNotFoundException) {
        val picker = Intent(Intent.ACTION_OPEN_DOCUMENT)
            .setType("*/*")
            .putExtra(
                DocumentsContract.EXTRA_INITIAL_URI,
                DocumentsContract.buildDocumentUri(Native.AUTHORITY, "/"),
            )
        context.startActivity(picker)
    }
}
