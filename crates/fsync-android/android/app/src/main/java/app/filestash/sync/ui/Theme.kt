package app.filestash.sync.ui

import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

val ConnectedGreen = Color(0xFF63D9B1)

private val DarkColors = darkColorScheme(
    primary = Color(0xFF9AD1ED),
    onPrimary = Color(0xFF14262F),
    primaryContainer = Color(0xFF2A4456),
    onPrimaryContainer = Color(0xFFC5E2F1),
    secondary = Color(0xFFA9C4D4),
    onSecondary = Color(0xFF12242D),
    secondaryContainer = Color(0xFF466372),
    onSecondaryContainer = Color(0xFFC5E2F1),
    background = Color(0xFF24272A),
    onBackground = Color(0xFFE5E7E9),
    surface = Color(0xFF24272A),
    onSurface = Color(0xFFE5E7E9),
    surfaceVariant = Color(0xFF33383D),
    onSurfaceVariant = Color(0xFFA9B1B8),
    outline = Color(0xFF6E7880),
    error = Color(0xFFF26D6D),
    onError = Color(0xFF2B1111),
)

@Composable
fun FilestashTheme(content: @Composable () -> Unit) {
    MaterialTheme(colorScheme = DarkColors, content = content)
}
