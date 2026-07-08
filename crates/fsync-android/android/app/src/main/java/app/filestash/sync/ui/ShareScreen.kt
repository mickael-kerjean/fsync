package app.filestash.sync.ui

import android.content.Context
import android.graphics.BitmapFactory
import android.net.Uri
import android.provider.OpenableColumns
import android.widget.Toast
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.ImageBitmap
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import app.filestash.core.EntryKind
import app.filestash.sync.Native
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

@Composable
fun ShareScreen(uris: List<Uri>, onDone: () -> Unit, modifier: Modifier = Modifier) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val names = remember { uris.map { displayName(context, it) } }
    val single = uris.size == 1
    var name by remember { mutableStateOf(names.first()) }
    var location by remember { mutableStateOf("/") }
    var uploading by remember { mutableStateOf(false) }
    var picking by remember { mutableStateOf(false) }

    Column(modifier = modifier.fillMaxSize()) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .background(MaterialTheme.colorScheme.surfaceContainer)
                .padding(horizontal = 8.dp, vertical = 8.dp),
        ) {
            IconButton(onClick = onDone, enabled = !uploading) {
                Text("✕", style = MaterialTheme.typography.titleMedium)
            }
            Text(
                "Upload to Drive",
                style = MaterialTheme.typography.titleLarge,
                modifier = Modifier
                    .weight(1f)
                    .padding(start = 4.dp),
            )
            Button(
                onClick = {
                    uploading = true
                    scope.launch(Dispatchers.IO) {
                        try {
                            for ((index, uri) in uris.withIndex()) {
                                val fileName =
                                    uniquify(context, location, if (single) name.trim() else names[index])
                                val target = location.trimEnd('/') + "/" + fileName
                                val local = Native.withReauth(context) { it.create(target) }
                                context.contentResolver.openInputStream(uri)?.use { input ->
                                    File(local).outputStream().use { input.copyTo(it) }
                                } ?: throw java.io.IOException("cannot read the shared content")
                                Native.withReauth(context) { it.saved(target) }
                            }
                            Native.client?.flush(120_000u)
                            withContext(Dispatchers.Main) {
                                val what = if (single) name.trim() else "${uris.size} files"
                                Toast.makeText(context, "Uploaded $what", Toast.LENGTH_SHORT).show()
                                onDone()
                            }
                        } catch (e: Exception) {
                            withContext(Dispatchers.Main) {
                                uploading = false
                                Toast.makeText(context, e.message ?: "Upload failed", Toast.LENGTH_LONG).show()
                            }
                        }
                    }
                },
                enabled = !uploading && name.isNotBlank(),
            ) {
                if (uploading) {
                    CircularProgressIndicator(modifier = Modifier.size(18.dp), strokeWidth = 2.dp)
                } else {
                    Text("Upload")
                }
            }
        }
        Column(
            modifier = Modifier
                .fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            Preview(uris.first())
            OutlinedTextField(
                value = if (single) name else names.joinToString(", "),
                onValueChange = { if (single) name = it },
                label = { Text(if (single) "Filename" else "Filenames") },
                singleLine = true,
                readOnly = !single,
                enabled = !uploading,
                modifier = Modifier.fillMaxWidth(),
            )
            OutlinedTextField(
                value = location, onValueChange = {},
                label = { Text("Location") }, singleLine = true,
                readOnly = true,
                trailingIcon = {
                    TextButton(onClick = { picking = true }, enabled = !uploading) { Text("Change") }
                },
                modifier = Modifier.fillMaxWidth(),
            )
        }
    }

    if (picking) {
        FolderPicker(
            start = location,
            onSelect = {
                location = it
                picking = false
            },
            onDismiss = { picking = false },
        )
    }
}

@Composable
private fun Preview(uri: Uri) {
    val context = LocalContext.current
    var image by remember { mutableStateOf<ImageBitmap?>(null) }
    LaunchedEffect(uri) {
        image = withContext(Dispatchers.IO) { loadPreview(context, uri) }
    }
    image?.let {
        Box(modifier = Modifier.fillMaxWidth(), contentAlignment = Alignment.Center) {
            Image(
                bitmap = it,
                contentDescription = null,
                modifier = Modifier
                    .heightIn(max = 280.dp)
                    .aspectRatio(it.width.toFloat() / it.height.toFloat().coerceAtLeast(1f))
                    .clip(MaterialTheme.shapes.medium),
            )
        }
        return
    }
    Box(
        modifier = Modifier
            .fillMaxWidth()
            .height(120.dp)
            .clip(MaterialTheme.shapes.medium)
            .background(MaterialTheme.colorScheme.surfaceVariant),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            context.contentResolver.getType(uri) ?: "file",
            style = MaterialTheme.typography.bodyMedium,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

@Composable
private fun FolderPicker(start: String, onSelect: (String) -> Unit, onDismiss: () -> Unit) {
    val context = LocalContext.current
    var dir by remember { mutableStateOf(start) }
    var dirs by remember { mutableStateOf<List<String>?>(null) }
    LaunchedEffect(dir) {
        dirs = null
        dirs = withContext(Dispatchers.IO) {
            runCatching {
                Native.withReauth(context) { it.ls(dir) }
                    .filter { entry -> entry.kind == EntryKind.DIRECTORY }
                    .map { entry -> entry.name }
                    .sorted()
            }.getOrDefault(emptyList())
        }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = {
            Text(if (dir == "/") "Filestash" else dir.trimEnd('/').substringAfterLast('/'))
        },
        text = {
            Column(
                modifier = Modifier
                    .heightIn(max = 320.dp)
                    .verticalScroll(rememberScrollState()),
            ) {
                if (dir != "/") {
                    PickerRow("..") { dir = dir.trimEnd('/').substringBeforeLast('/') + "/" }
                }
                when (val list = dirs) {
                    null -> CircularProgressIndicator(
                        modifier = Modifier
                            .padding(16.dp)
                            .align(Alignment.CenterHorizontally),
                    )
                    else -> list.forEach { child ->
                        PickerRow(child) { dir = dir.trimEnd('/') + "/" + child + "/" }
                    }
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { onSelect(dir) }) { Text("Select") }
        },
        dismissButton = {
            TextButton(onClick = onDismiss) { Text("Cancel") }
        },
    )
}

@Composable
private fun PickerRow(label: String, onClick: () -> Unit) {
    Text(
        label,
        style = MaterialTheme.typography.bodyLarge,
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(vertical = 10.dp),
    )
}

private fun uniquify(context: Context, dir: String, name: String): String {
    val stem = name.substringBeforeLast('.', name)
    val ext = name.substringAfterLast('.', "").let { if (it.isEmpty()) "" else ".$it" }
    var candidate = name
    var n = 1
    while (exists(context, dir.trimEnd('/') + "/" + candidate)) {
        candidate = "$stem ($n)$ext"
        n++
    }
    return candidate
}

private fun exists(context: Context, path: String): Boolean =
    runCatching { Native.withReauth(context) { it.stat(path) } }.isSuccess

private fun displayName(context: Context, uri: Uri): String =
    context.contentResolver
        .query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)
        ?.use { cursor -> if (cursor.moveToFirst()) cursor.getString(0) else null }
        ?: uri.lastPathSegment?.substringAfterLast('/')
        ?: "file"

private fun loadPreview(context: Context, uri: Uri): ImageBitmap? = runCatching {
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    context.contentResolver.openInputStream(uri)?.use {
        BitmapFactory.decodeStream(it, null, bounds)
    }
    var sample = 1
    while (bounds.outWidth / (sample * 2) > 1080) sample *= 2
    val options = BitmapFactory.Options().apply { inSampleSize = sample }
    context.contentResolver.openInputStream(uri)?.use {
        BitmapFactory.decodeStream(it, null, options)
    }?.asImageBitmap()
}.getOrNull()
