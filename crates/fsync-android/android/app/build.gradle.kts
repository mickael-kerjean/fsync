import org.jetbrains.kotlin.gradle.dsl.JvmTarget

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val workspaceRoot: File = rootDir.resolve("../../..").normalize()

val sdkDir: String? = System.getenv("ANDROID_HOME")
    ?: rootDir.resolve("local.properties").takeIf { it.exists() }
        ?.readLines()?.firstOrNull { it.startsWith("sdk.dir=") }
        ?.substringAfter("sdk.dir=")

android {
    namespace = "app.filestash.sync"
    compileSdk = 35

    defaultConfig {
        applicationId = "app.filestash.sync"
        minSdk = 26
        targetSdk = 35
        versionCode = 1
        versionName = "0.1.0"
    }

    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }

    buildFeatures {
        compose = true
    }

    sourceSets["main"].kotlin.srcDir(layout.buildDirectory.dir("generated/uniffi"))
}

kotlin {
    compilerOptions {
        jvmTarget.set(JvmTarget.JVM_17)
    }
}

val cargoHostBuild = tasks.register<Exec>("cargoHostBuild") {
    workingDir = workspaceRoot
    commandLine("cargo", "build", "-p", "fsync-android")
}

val generateUniffiBindings = tasks.register<Exec>("generateUniffiBindings") {
    dependsOn(cargoHostBuild)
    workingDir = workspaceRoot
    commandLine(
        "cargo", "run", "-p", "fsync-android", "--bin", "uniffi-bindgen", "--",
        "generate", "--library", "target/debug/libfsync_android.so",
        "--language", "kotlin", "--no-format",
        "--out-dir", layout.buildDirectory.dir("generated/uniffi").get().asFile.absolutePath,
    )
}

val cargoNdkBuild = tasks.register<Exec>("cargoNdkBuild") {
    workingDir = workspaceRoot
    sdkDir?.let { environment("ANDROID_HOME", it) }
    commandLine(
        "cargo", "ndk", "-t", "arm64-v8a", "-t", "x86_64",
        "-o", layout.projectDirectory.dir("src/main/jniLibs").asFile.absolutePath,
        "build", "--release", "-p", "fsync-android",
    )
}

tasks.named("preBuild") {
    dependsOn(generateUniffiBindings, cargoNdkBuild)
}

dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.05.00"))
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    implementation("androidx.navigation:navigation-compose:2.9.0")
    implementation("androidx.security:security-crypto:1.1.0-alpha07")
    implementation("net.java.dev.jna:jna:5.17.0@aar")
}
