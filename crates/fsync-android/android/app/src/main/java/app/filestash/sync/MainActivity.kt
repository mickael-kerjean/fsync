package app.filestash.sync

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.ui.Modifier
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import app.filestash.sync.ui.FilestashTheme
import app.filestash.sync.ui.HomeScreen
import app.filestash.sync.ui.LoginScreen

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Native.init(this)
        enableEdgeToEdge(
            statusBarStyle = SystemBarStyle.light(0, 0),
            navigationBarStyle = SystemBarStyle.light(0, 0),
        )
        setContent {
            FilestashTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background,
                ) {
                    val navController = rememberNavController()
                    NavHost(
                        navController = navController,
                        startDestination = if (Native.client != null) "home" else "login",
                        modifier = Modifier.safeDrawingPadding(),
                    ) {
                        composable("login") {
                            LoginScreen(onLoggedIn = {
                                navController.navigate("home") { popUpTo("login") { inclusive = true } }
                            })
                        }
                        composable("home") {
                            HomeScreen(onLoggedOut = {
                                navController.navigate("login") { popUpTo("home") { inclusive = true } }
                            })
                        }
                    }
                }
            }
        }
    }
}
