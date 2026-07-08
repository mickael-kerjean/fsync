package app.filestash.sync.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

val ConnectedGreen = Color(0xFF63D9B1)

private val LightColors = lightColorScheme(
    primary = Color(0xFF466372),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFC5E2F1),
    onPrimaryContainer = Color(0xFF24272A),
    secondary = Color(0xFF9AD1ED),
    onSecondary = Color(0xFF24272A),
    secondaryContainer = Color(0xFFC5E2F1),
    onSecondaryContainer = Color(0xFF24272A),
    background = Color(0xFFF9F9FA),
    onBackground = Color(0xFF343637),
    surface = Color(0xFFF9F9FA),
    onSurface = Color(0xFF343637),
    surfaceVariant = Color(0xFFEDEFF0),
    onSurfaceVariant = Color(0xFF555758),
    surfaceContainer = Color(0xFFF3F4F6),
    surfaceContainerHigh = Color(0xFFFAFAFA),
    outline = Color(0xFFC6C8CC),
    error = Color(0xFFF26D6D),
    onError = Color(0xFFFFFFFF),
)

@Composable
fun FilestashTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = LightColors, content = content)
}
