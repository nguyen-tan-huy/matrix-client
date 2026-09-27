buildscript {
    repositories {
        google()
        mavenCentral()
    }
    dependencies {
        classpath("com.android.tools.build:gradle:8.11.0")
        // Bumped from 1.9.25 for `org.unifiedpush.android:connector`
        // (see app/build.gradle.kts) — every published version of that
        // library, even its oldest release on Maven Central, is built
        // against a Kotlin stdlib whose metadata a 1.9.x compiler can't
        // read at all (hard compile error, not just a warning). 2.0.21 is
        // the oldest 2.x line that satisfies it.
        classpath("org.jetbrains.kotlin:kotlin-gradle-plugin:2.0.21")
    }
}

allprojects {
    repositories {
        google()
        mavenCentral()
    }
}

tasks.register("clean").configure {
    delete("build")
}

