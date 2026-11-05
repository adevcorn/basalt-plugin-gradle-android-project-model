# gradle-android-project-model

Tier-0 project-model, build-system, and test-runner plugin for Gradle and
Android workspaces in Basalt.

## Capabilities

- `PROJECT_MODEL` / `project-model:gradle-android`: emits the generic
  project-model schema for the root Gradle workspace and every included
  module.
- `BUILD_SYSTEM`: exposes `compileDebugSources`.
- `TEST_RUNNER`: exposes `testDebugUnitTest`.
- `REVIEW_ACTIONS`: build, unit test, and `lintDebug`, with Kotlin compiler
  and Gradle test-output parsing.

## Project model

- `settings.gradle(.kts)` is discovered at the workspace root, under
  `android/`, or anywhere in the tree.
- `rootProject.name` and every `include(":module")` are parsed; nested module
  paths (`:feature:calendar` -> `feature/calendar`) are supported.
- Each module's `build.gradle(.kts)` is classified as `android-app`,
  `android-library`, `jvm-library`, or `gradle-module` from its plugin ids.
- `AndroidManifest.xml` contributes `has-services`, `has-receivers`,
  `exposes-activities`, and `declares-permissions`. AIDL usage adds
  `has-aidl`; Compose adds `uses-compose`.
- Owners are workspace-relative, so an `add-helper` workspace produces paths
  such as `android/app/src/main/` and `android/app/build.gradle.kts`.

## Running Gradle

- If `gradlew.bat` exists, the plan runs `cmd /C gradlew.bat ...`; if
  `gradlew` exists it is executed directly. Otherwise the system `gradle` is
  used, so the plugin works in checkouts without a wrapper (as in
  `add-helper`).
- A Gradle project nested under `android/` is passed with `-p android`, so the
  action still runs from the workspace root.
- Without an Android SDK the action still runs and surfaces Gradle's own
  error rather than failing at plan time.

## Review output parsing

- `e: file:///C:/.../Foo.kt:12:34 message` and `w: ...` become `build` /
  `diagnostic` entries with the offending file.
- `> Task :app:testDebugUnitTest FAILED`, `FAILURE: ...`, and
  `Tests run: N, Failures: M` are surfaced; the summary closes the run with
  exit code 0 or 1.
- ANSI colour sequences are stripped.

## Build

```bash
cargo build --release --target wasm32-unknown-unknown
```

Artifact:
`target/wasm32-unknown-unknown/release/gradle_android_project_model.wasm`.
