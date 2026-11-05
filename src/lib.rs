//! Tier-0 project-model plugin for Gradle / Android workspaces.
//!
//! Emits the shared generic project-model JSON schema under
//! `project-model:gradle-android`, exposing the root Gradle workspace plus
//! every included module (app, library, JVM module) with Android source,
//! test, and build roots.
//!
//! Review actions run Gradle tasks (`compileDebugSources`, `testDebugUnitTest`,
//! `lintDebug`) and map Kotlin compiler diagnostics (`e:` / `w:`) and test
//! summaries to agent events. When a wrapper is present it is used; otherwise
//! the system `gradle` executable is used. Missing Android SDKs degrade
//! gracefully: the action still runs and reports Gradle's own error.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use basalt_plugin_sdk::prelude::*;

basalt_plugin_meta! {
    name:         "gradle-android-project-model",
    version:      "0.1.1",
    hook_flags:   CAP_PROJECT_MODEL | CAP_REVIEW_ACTIONS | CAP_BUILD_SYSTEM | CAP_TEST_RUNNER | CAP_SHADOW_EXCLUDES,
    provides:     "project-model:gradle-android",
    requires:     "",
    file_globs:   "",
    activates_on: "settings.gradle.kts\nsettings.gradle\nbuild.gradle.kts\nbuild.gradle\n**/settings.gradle.kts\n**/settings.gradle\n**/build.gradle.kts\n**/build.gradle\n**/AndroidManifest.xml\n**/*.aidl",
    activation_events: "",
}

/// Gradle/Android build outputs and release artifacts.
#[basalt_plugin]
fn shadow_excludes(_root: &str) -> Vec<u8> {
    br#"["**/.gradle/**","**/build/**","**/*.apk","**/*.aab"]"#.to_vec()
}

#[cfg(target_arch = "wasm32")]
extern "C" {
    fn basalt_read_file(path_ptr: i32, path_len: i32, out_ptr: i32, out_cap: i32) -> i32;
    fn basalt_list_files(root_ptr: i32, root_len: i32, out_ptr: i32, out_cap: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_read_file(
    _path_ptr: i32,
    _path_len: i32,
    _out_ptr: i32,
    _out_cap: i32,
) -> i32 {
    -1
}

#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
unsafe extern "C" fn basalt_list_files(
    _root_ptr: i32,
    _root_len: i32,
    _out_ptr: i32,
    _out_cap: i32,
) -> i32 {
    -1
}

const FILE_BUF_SIZE: usize = 4 * 1024 * 1024;
const LIST_BUF_SIZE: usize = 8 * 1024 * 1024;

static mut FILE_BUF: [u8; FILE_BUF_SIZE] = [0u8; FILE_BUF_SIZE];
static mut LIST_BUF: [u8; LIST_BUF_SIZE] = [0u8; LIST_BUF_SIZE];

#[derive(Debug, Clone)]
struct ModuleInfo {
    /// Workspace-relative directory, e.g. `app` or `feature/ui`.
    path: String,
    name: String,
    kind: String,
    subtype: String,
    capabilities: Vec<String>,
    namespace: Option<String>,
    application_id: Option<String>,
}

#[basalt_plugin]
fn build_project_model(root: &str) -> Vec<u8> {
    let Some(settings) = resolve_settings(root) else {
        return Vec::new();
    };
    let project_dir = parent_dir(&settings);
    let project_prefix = relative_to(root, &project_dir);
    let settings_contents = read_host_file(&settings).unwrap_or_default();
    let (root_name, includes) = parse_settings(&settings_contents);
    let display_name = root_name.unwrap_or_else(|| {
        let dir = file_name(&project_dir);
        if dir.is_empty() {
            "gradle-workspace".to_string()
        } else {
            dir.to_string()
        }
    });

    let all_files = list_host_files(&project_dir);

    let mut modules = Vec::new();
    for include in includes {
        let module_dir = join_path(&project_dir, &include);
        let module_path = with_prefix(&project_prefix, &include);
        let module_prefix = format!("{include}/");
        let module_has_aidl = all_files.iter().any(|p| {
            let p = p.replace('\\', "/");
            p.starts_with(&module_prefix) && p.ends_with(".aidl")
        });
        let build_file = ["build.gradle.kts", "build.gradle"]
            .iter()
            .map(|name| join_path(&module_dir, name))
            .find(|path| read_host_file(path).is_some());
        let contents = build_file
            .as_deref()
            .and_then(read_host_file)
            .unwrap_or_default();
        let manifest_path = join_path(&module_dir, "src/main/AndroidManifest.xml");
        let manifest = read_host_file(&manifest_path);

        modules.push(parse_module(
            &module_path,
            &contents,
            manifest.as_deref(),
            module_has_aidl,
            build_file.is_some(),
        ));
    }

    emit_model(
        root,
        &relative_to(root, &settings),
        &display_name,
        &modules,
        &project_prefix,
    )
    .into_bytes()
}

/// Locate the Gradle settings file. `root` may be the workspace root, the
/// Gradle project directory, or the settings file itself.
fn resolve_settings(root: &str) -> Option<String> {
    let name = file_name(root);
    if name == "settings.gradle.kts" || name == "settings.gradle" {
        return Some(root.to_string());
    }

    for candidate in [
        join_path(root, "settings.gradle.kts"),
        join_path(root, "settings.gradle"),
        join_path(root, "android/settings.gradle.kts"),
        join_path(root, "android/settings.gradle"),
    ] {
        if read_host_file(&candidate).is_some() {
            return Some(candidate);
        }
    }

    let mut best: Option<(usize, String)> = None;
    for rel in list_host_files(root) {
        let rel = normalize_rel_path(&rel);
        let file = file_name(&rel);
        if file != "settings.gradle.kts" && file != "settings.gradle" {
            continue;
        }
        let depth = rel.matches('/').count();
        if best.as_ref().map(|(d, _)| depth < *d).unwrap_or(true) {
            best = Some((depth, join_path(root, &rel)));
        }
    }
    best.map(|(_, path)| path)
}

// ── Gradle file parsing ─────────────────────────────────────────────────────

fn parse_settings(contents: &str) -> (Option<String>, Vec<String>) {
    let mut root_name = None;
    let mut includes = Vec::new();

    for line in contents.lines() {
        let line = strip_line_comment(line).trim();
        if line.starts_with("rootProject.name") {
            if let Some(value) = first_quoted(line) {
                root_name = Some(value);
            }
            continue;
        }
        if !line.contains("include") || line.contains("includeBuild") {
            continue;
        }
        for (idx, segment) in line.split('"').enumerate() {
            // Odd segments are the contents of quoted strings.
            if idx % 2 == 0 {
                continue;
            }
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            let path = segment.trim_start_matches(':').replace(':', "/");
            if !path.is_empty() && !includes.contains(&path) {
                includes.push(path);
            }
        }
    }

    (root_name, includes)
}

fn strip_line_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in line.char_indices() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '/' if !in_single && !in_double && line[idx..].starts_with("//") => {
                return &line[..idx];
            }
            _ => {}
        }
    }
    line
}

fn first_quoted(line: &str) -> Option<String> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    let value = line[start..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn parse_module(
    path: &str,
    build_contents: &str,
    manifest: Option<&str>,
    workspace_has_aidl: bool,
    build_file_exists: bool,
) -> ModuleInfo {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let is_app = build_contents.contains("com.android.application")
        || build_contents.contains("android.application");
    let is_android_library = build_contents.contains("com.android.library")
        || build_contents.contains("android.library");
    let is_kotlin_jvm = build_contents.contains("org.jetbrains.kotlin.jvm");

    let (kind, subtype) = if is_app {
        ("app", "android-app")
    } else if is_android_library {
        ("library", "android-library")
    } else if is_kotlin_jvm {
        ("library", "jvm-library")
    } else if build_file_exists {
        ("module", "gradle-module")
    } else {
        ("module", "gradle-module")
    };

    let mut capabilities = Vec::new();
    if is_app {
        capabilities.push("is-executable".to_string());
    } else {
        capabilities.push("is-library".to_string());
    }
    if is_app || is_android_library {
        capabilities.push("targets-android".to_string());
    }
    if build_contents.contains("compose = true")
        || build_contents.contains("org.jetbrains.kotlin.plugin.compose")
        || build_contents.contains("androidx.compose")
    {
        capabilities.push("uses-compose".to_string());
    }
    if workspace_has_aidl || build_contents.contains("aidl = true") {
        capabilities.push("has-aidl".to_string());
    }
    if let Some(manifest) = manifest {
        if manifest.contains("<service") {
            capabilities.push("has-services".to_string());
        }
        if manifest.contains("<receiver") {
            capabilities.push("has-receivers".to_string());
        }
        if manifest.contains("<activity") {
            capabilities.push("exposes-activities".to_string());
        }
        if manifest.contains("<uses-permission") {
            capabilities.push("declares-permissions".to_string());
        }
    }

    ModuleInfo {
        path: path.to_string(),
        name,
        kind: kind.to_string(),
        subtype: subtype.to_string(),
        capabilities,
        namespace: parse_gradle_string(build_contents, "namespace"),
        application_id: parse_gradle_string(build_contents, "applicationId"),
    }
}

/// Find `key = "value"` in a Gradle Kotlin DSL file.
fn parse_gradle_string(contents: &str, key: &str) -> Option<String> {
    for line in contents.lines() {
        let line = strip_line_comment(line).trim();
        if !line.starts_with(key) {
            continue;
        }
        let rest = line[key.len()..].trim_start();
        if !rest.starts_with('=') {
            continue;
        }
        if let Some(value) = first_quoted(rest) {
            return Some(value);
        }
    }
    None
}

// ── Model emission ──────────────────────────────────────────────────────────

fn emit_model(
    root: &str,
    settings_path: &str,
    display_name: &str,
    modules: &[ModuleInfo],
    project_prefix: &str,
) -> String {
    let workspace_id = sanitize_id(display_name);
    let workspace_build_roots = vec![
        with_prefix(project_prefix, ".gradle/"),
        with_prefix(project_prefix, "build/"),
    ];
    let workspace_target = format!(
        concat!(
            "{{",
            "\"id\":\"{}\",",
            "\"name\":\"{}\",",
            "\"kind\":\"workspace\",",
            "\"subtype\":\"gradle-workspace\",",
            "\"capabilities\":[\"is-workspace\"],",
            "\"language\":\"kotlin\",",
            "\"source_roots\":[],",
            "\"test_roots\":[],",
            "\"build_roots\":[{}]",
            "}}"
        ),
        escape_json(&workspace_id),
        escape_json(display_name),
        quote_list(&workspace_build_roots),
    );
    let workspace = format!(
        concat!(
            "{{",
            "\"id\":\"{}\",",
            "\"name\":\"{}\",",
            "\"kind\":\"workspace\",",
            "\"subtype\":\"gradle-workspace\",",
            "\"capabilities\":[\"is-workspace\"],",
            "\"manifest_path\":\"{}\",",
            "\"source_roots\":[],",
            "\"test_roots\":[],",
            "\"build_roots\":[{}],",
            "\"targets\":[{}]",
            "}}"
        ),
        escape_json(&workspace_id),
        escape_json(display_name),
        escape_json(settings_path),
        quote_list(&workspace_build_roots),
        workspace_target,
    );

    let mut projects = vec![workspace];
    let mut owners = vec![
        owner_json(
            &with_prefix(project_prefix, "settings.gradle.kts"),
            "exact",
            display_name,
            &workspace_id,
            "manifest",
        ),
        owner_json(
            &with_prefix(project_prefix, "settings.gradle"),
            "exact",
            display_name,
            &workspace_id,
            "manifest",
        ),
        owner_json(
            &with_prefix(project_prefix, ".gradle/"),
            "prefix",
            display_name,
            &workspace_id,
            "build",
        ),
        owner_json(
            &with_prefix(project_prefix, "build/"),
            "prefix",
            display_name,
            &workspace_id,
            "build",
        ),
        owner_json(
            &with_prefix(project_prefix, "gradle/"),
            "prefix",
            display_name,
            &workspace_id,
            "build",
        ),
    ];

    for module in modules {
        let id = sanitize_id(&module.path.replace('/', "-"));
        let android = module.subtype.starts_with("android");
        let mut source_roots = vec![
            format!("{}/src/main/java/", module.path),
            format!("{}/src/main/kotlin/", module.path),
        ];
        let mut test_roots = vec![
            format!("{}/src/test/java/", module.path),
            format!("{}/src/test/kotlin/", module.path),
        ];
        if android {
            source_roots.push(format!("{}/src/main/aidl/", module.path));
            source_roots.push(format!("{}/src/main/res/", module.path));
            test_roots.push(format!("{}/src/androidTest/java/", module.path));
            test_roots.push(format!("{}/src/androidTest/kotlin/", module.path));
        }
        let build_roots = vec![format!("{}/build/", module.path)];

        let target = format!(
            concat!(
                "{{",
                "\"id\":\"{}\",",
                "\"name\":\"{}\",",
                "\"kind\":\"{}\",",
                "\"subtype\":\"{}\",",
                "\"capabilities\":[{}],",
                "\"language\":\"kotlin\",",
                "\"source_roots\":[{}],",
                "\"test_roots\":[{}],",
                "\"build_roots\":[{}]",
                "}}"
            ),
            escape_json(&id),
            escape_json(&module.name),
            escape_json(&module.kind),
            escape_json(&module.subtype),
            quote_list(&module.capabilities),
            quote_list(&source_roots),
            quote_list(&test_roots),
            quote_list(&build_roots),
        );

        let manifest_path = if android {
            format!(
                "\"{}/src/main/AndroidManifest.xml\"",
                escape_json(&module.path)
            )
        } else {
            "null".to_string()
        };
        let metadata = format!(
            "{{\"namespace\":{},\"application_id\":{}}}",
            opt_json(&module.namespace),
            opt_json(&module.application_id),
        );

        projects.push(format!(
            concat!(
                "{{",
                "\"id\":\"{}\",",
                "\"name\":\"{}\",",
                "\"kind\":\"{}\",",
                "\"subtype\":\"{}\",",
                "\"capabilities\":[{}],",
                "\"manifest_path\":{},",
                "\"metadata\":{},",
                "\"source_roots\":[{}],",
                "\"test_roots\":[{}],",
                "\"build_roots\":[{}],",
                "\"targets\":[{}]",
                "}}"
            ),
            escape_json(&id),
            escape_json(&module.name),
            escape_json(&module.kind),
            escape_json(&module.subtype),
            quote_list(&module.capabilities),
            manifest_path,
            metadata,
            quote_list(&source_roots),
            quote_list(&test_roots),
            quote_list(&build_roots),
            target,
        ));

        owners.push(owner_json(
            &format!("{}/build.gradle.kts", module.path),
            "exact",
            &module.name,
            &id,
            "manifest",
        ));
        owners.push(owner_json(
            &format!("{}/build.gradle", module.path),
            "exact",
            &module.name,
            &id,
            "manifest",
        ));
        owners.push(owner_json(
            &format!("{}/src/main/", module.path),
            "prefix",
            &module.name,
            &id,
            "source",
        ));
        owners.push(owner_json(
            &format!("{}/src/test/", module.path),
            "prefix",
            &module.name,
            &id,
            "test",
        ));
        if android {
            owners.push(owner_json(
                &format!("{}/src/androidTest/", module.path),
                "prefix",
                &module.name,
                &id,
                "test",
            ));
        }
        owners.push(owner_json(
            &format!("{}/build/", module.path),
            "prefix",
            &module.name,
            &id,
            "build",
        ));
    }

    format!(
        concat!(
            "{{",
            "\"schema_version\":1,",
            "\"ecosystem\":\"gradle-android\",",
            "\"root\":\"{}\",",
            "\"display_name\":\"{}\",",
            "\"primary_manifest_path\":\"{}\",",
            "\"projects\":[{}],",
            "\"owners\":[{}]",
            "}}"
        ),
        escape_json(root),
        escape_json(display_name),
        escape_json(settings_path),
        projects.join(","),
        owners.join(","),
    )
}

fn owner_json(path: &str, match_kind: &str, label: &str, project_id: &str, kind: &str) -> String {
    format!(
        "{{\"path\":\"{}\",\"match_kind\":\"{}\",\"label\":\"{}\",\"project_id\":\"{}\",\"target_id\":\"{}\",\"kind\":\"{}\"}}",
        escape_json(path),
        escape_json(match_kind),
        escape_json(label),
        escape_json(project_id),
        escape_json(project_id),
        escape_json(kind),
    )
}

// ── Host I/O ────────────────────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
fn read_host_file(path: &str) -> Option<String> {
    let buf: &mut [u8; FILE_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(FILE_BUF) };
    let n = unsafe {
        basalt_read_file(
            path.as_ptr() as i32,
            path.len() as i32,
            buf.as_mut_ptr() as i32,
            FILE_BUF_SIZE as i32,
        )
    };
    if n <= 0 {
        return None;
    }
    Some(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(not(target_arch = "wasm32"))]
fn read_host_file(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

#[cfg(target_arch = "wasm32")]
fn list_host_files(root: &str) -> Vec<String> {
    let buf: &mut [u8; LIST_BUF_SIZE] = unsafe { &mut *core::ptr::addr_of_mut!(LIST_BUF) };
    let n = unsafe {
        basalt_list_files(
            root.as_ptr() as i32,
            root.len() as i32,
            buf.as_mut_ptr() as i32,
            LIST_BUF_SIZE as i32,
        )
    };
    if n <= 0 {
        return Vec::new();
    }
    String::from_utf8_lossy(&buf[..n as usize])
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn list_host_files(root: &str) -> Vec<String> {
    fn walk(base: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>, depth: usize) {
        if depth > 8 || out.len() > 20_000 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(
                name.as_str(),
                ".git" | ".basalt" | "build" | ".gradle" | "node_modules" | "target"
            ) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out, depth + 1);
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }

    let base = std::path::Path::new(root);
    let mut out = Vec::new();
    walk(base, base, &mut out, 0);
    out
}

// ── Review actions ──────────────────────────────────────────────────────────

#[basalt_plugin]
fn review_actions(workspace_root: &str, _session_workspace: &str) -> Vec<ReviewActionDescriptor> {
    if resolve_settings(workspace_root).is_none() {
        return Vec::new();
    }
    vec![
        ReviewActionDescriptor {
            id: "gradle-build".into(),
            title: "Assemble Debug APK".into(),
            kind: ReviewActionKind::Build,
            ecosystem: "gradle-android".into(),
            command_preview: "gradle compileDebugSources".into(),
            mutates_workspace: false,
            priority: 100,
        },
        ReviewActionDescriptor {
            id: "gradle-test".into(),
            title: "Run Unit Tests".into(),
            kind: ReviewActionKind::Test,
            ecosystem: "gradle-android".into(),
            command_preview: "gradle testDebugUnitTest".into(),
            mutates_workspace: false,
            priority: 100,
        },
        ReviewActionDescriptor {
            id: "gradle-lint".into(),
            title: "Run Android Lint".into(),
            kind: ReviewActionKind::Lint,
            ecosystem: "gradle-android".into(),
            command_preview: "gradle lintDebug".into(),
            mutates_workspace: false,
            priority: 80,
        },
    ]
}

#[basalt_plugin]
fn review_action_plan(
    action_id: &str,
    _workspace_root: &str,
    session_workspace: &str,
) -> Option<ReviewActionExecutionPlan> {
    let (executable, mut args) = gradle_invocation(session_workspace);
    let (task, category) = match action_id {
        "gradle-build" => ("compileDebugSources", "build"),
        "gradle-test" => ("testDebugUnitTest", "test"),
        "gradle-lint" => ("lintDebug", "lint"),
        _ => return None,
    };
    args.push(task.to_string());
    args.push("--console=plain".to_string());
    Some(ReviewActionExecutionPlan {
        executable,
        args,
        env: Vec::new(),
        cwd_mode: ReviewActionCwdMode::SessionWorkspace,
        output_category: category.to_string(),
    })
}

/// Resolve the Gradle executable and project-directory prefix.
///
/// `cmd /C gradlew.bat` is used on Windows-style wrappers because batch files
/// cannot be spawned directly. Without a wrapper the system `gradle` is used.
fn gradle_invocation(session_workspace: &str) -> (String, Vec<String>) {
    let project_dir = gradle_project_dir(session_workspace);
    let mut args = Vec::new();
    if !project_dir.is_empty() {
        args.push("-p".to_string());
        args.push(project_dir.clone());
    }

    let base = if project_dir.is_empty() {
        session_workspace.to_string()
    } else {
        join_path(session_workspace, &project_dir)
    };

    let bat = join_path(&base, "gradlew.bat");
    if read_host_file(&bat).is_some() {
        let mut cmd_args = vec!["/C".to_string(), bat];
        cmd_args.extend(args);
        return ("cmd".to_string(), cmd_args);
    }

    let sh = join_path(&base, "gradlew");
    if read_host_file(&sh).is_some() {
        return (sh, args);
    }

    ("gradle".to_string(), args)
}

/// Workspace-relative directory that contains the Gradle settings file.
fn gradle_project_dir(session_workspace: &str) -> String {
    for candidate in ["", "android"] {
        for settings in ["settings.gradle.kts", "settings.gradle"] {
            let rel = if candidate.is_empty() {
                settings.to_string()
            } else {
                format!("{candidate}/{settings}")
            };
            if read_host_file(&join_path(session_workspace, &rel)).is_some() {
                return candidate.to_string();
            }
        }
    }

    for rel in list_host_files(session_workspace) {
        let rel = normalize_rel_path(&rel);
        let file = file_name(&rel);
        if file == "settings.gradle.kts" || file == "settings.gradle" {
            return parent_dir(&rel);
        }
    }

    String::new()
}

#[basalt_plugin]
fn review_action_parse_line(
    action_id: &str,
    line: &[u8],
    state: &[u8],
) -> (Vec<u8>, Vec<AgentEvent>) {
    let mut counter = read_counter(state);
    let Ok(line_str) = core::str::from_utf8(line) else {
        return (encode_counter(counter), Vec::new());
    };
    let line_str = strip_ansi(line_str);
    let trimmed = line_str.trim();
    if trimmed.is_empty() {
        return (encode_counter(counter), Vec::new());
    }

    let mut events = Vec::new();

    if let Some((severity, path, line, col, message)) = parse_kotlin_diagnostic(trimmed) {
        let json = diagnostic_json(
            &path,
            line,
            col,
            if severity == "e" { "error" } else { "warning" },
            &message,
        );
        events.push(AgentEvent::NewEntry {
            vendor_id: format!("gradle-{counter}"),
            tool: "gradle".into(),
            category: if severity == "e" {
                "build"
            } else {
                "diagnostic"
            }
            .into(),
            raw_cmd: json,
            file_paths: Vec::new(),
        });
        counter += 1;
    } else if let Some((task_name, failed)) = parse_gradle_task_line(trimmed) {
        if failed {
            events.push(AgentEvent::NewEntry {
                vendor_id: format!("gradle-{counter}"),
                tool: "gradle".into(),
                category: if action_id == "gradle-test" {
                    "test".into()
                } else {
                    "build".into()
                },
                raw_cmd: format!("Task {task_name} FAILED"),
                file_paths: Vec::new(),
            });
            counter += 1;
        }
    } else if trimmed.starts_with("FAILURE:") || trimmed.starts_with("* What went wrong:") {
        events.push(AgentEvent::NewEntry {
            vendor_id: format!("gradle-{counter}"),
            tool: "gradle".into(),
            category: "build".into(),
            raw_cmd: trimmed.to_string(),
            file_paths: Vec::new(),
        });
        counter += 1;
    } else if let Some((run, failures)) = parse_test_summary(trimmed) {
        events.push(AgentEvent::CloseEntry {
            vendor_id: format!("gradle-test-{counter}"),
            exit_code: if failures > 0 { 1 } else { 0 },
            output_lines: vec![format!("tests run: {run}, failures: {failures}")],
        });
        counter += 1;
    }

    (encode_counter(counter), events)
}

/// Parse a Gradle task execution line (e.g. `> Task :app:compileDebugKotlin`,
/// `Task :app:preBuild UP-TO-DATE`, `:app:compileDebugSources`, etc.).
/// Returns `Some((task_name, failed))`.
fn parse_gradle_task_line(line: &str) -> Option<(&str, bool)> {
    let candidate = line.strip_prefix('>').map(|s| s.trim_start()).unwrap_or(line);
    let task = if let Some(rest) = candidate.strip_prefix("Task ") {
        rest
    } else if candidate.starts_with(':') && !candidate.starts_with(": ") {
        candidate
    } else {
        return None;
    };
    let failed = task.ends_with(" FAILED");
    let task_name = if failed {
        task.strip_suffix(" FAILED").unwrap_or(task).trim()
    } else {
        task.trim()
    };
    Some((task_name, failed))
}

/// Parse a Kotlin compiler line:
/// `e: file:///C:/ws/app/File.kt:12:34 Unresolved reference: foo`
fn parse_kotlin_diagnostic(line: &str) -> Option<(String, String, u32, u32, String)> {
    let (severity, rest) = if let Some(rest) = line.strip_prefix("e: ") {
        ("e", rest)
    } else if let Some(rest) = line.strip_prefix("w: ") {
        ("w", rest)
    } else {
        return None;
    };
    let rest = rest.strip_prefix("file://").unwrap_or(rest);
    let (location, message) = rest.split_once(' ').unwrap_or((rest, ""));
    let (path, line_no, col) = split_location(location)?;
    Some((
        severity.to_string(),
        normalize_uri_path(&path),
        line_no,
        col,
        message.trim().to_string(),
    ))
}

/// `DiagnosticItem` JSON understood by `basalt_core::build_runner`.
fn diagnostic_json(file: &str, line: u32, col: u32, severity: &str, message: &str) -> String {
    format!(
        "{{\"file\":\"{}\",\"line\":{},\"col\":{},\"severity\":\"{}\",\"message\":\"{}\"}}",
        escape_json(file),
        line,
        col,
        severity,
        escape_json(message),
    )
}

/// Strip the leading `/` from `/C:/...` URI paths on Windows.
fn normalize_uri_path(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() > 2 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        path[1..].to_string()
    } else {
        path.to_string()
    }
}

/// Parse `Tests run: 12, Failures: 1, Errors: 0, Skipped: 0`.
fn parse_test_summary(line: &str) -> Option<(u32, u32)> {
    let lower = line.to_ascii_lowercase();
    if !lower.contains("tests run:") {
        return None;
    }
    let run = number_after(line, "Tests run:")?;
    let failures = number_after(line, "Failures:").unwrap_or(0);
    Some((run, failures))
}

fn number_after(line: &str, label: &str) -> Option<u32> {
    let lower = line.to_ascii_lowercase();
    let idx = lower.find(&label.to_ascii_lowercase())? + label.len();
    let rest = line[idx..].trim_start_matches([' ', '=']);
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn read_counter(state: &[u8]) -> u32 {
    if state.len() >= 4 {
        u32::from_le_bytes([state[0], state[1], state[2], state[3]])
    } else {
        0
    }
}

fn encode_counter(n: u32) -> Vec<u8> {
    n.to_le_bytes().to_vec()
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Split `path:line:col` while tolerating Windows drive letters.
fn split_location(location: &str) -> Option<(String, u32, u32)> {
    let mut parts = location.rsplitn(3, ':');
    let col: u32 = parts.next()?.trim().parse().ok()?;
    let line: u32 = parts.next()?.trim().parse().ok()?;
    let path = parts.next()?.trim();
    if path.is_empty() {
        None
    } else {
        Some((path.to_string(), line, col))
    }
}

// ── Small path/JSON helpers ─────────────────────────────────────────────────

fn normalize_rel_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// Join a workspace-relative prefix with a relative subpath.
fn with_prefix(prefix: &str, path: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        path.to_string()
    } else {
        format!("{prefix}/{}", path.trim_start_matches('/'))
    }
}

/// Workspace-relative path of `path` under `base` ("" when equal).
fn relative_to(base: &str, path: &str) -> String {
    let base = base.replace('\\', "/");
    let base = base.trim_end_matches('/');
    let path = path.replace('\\', "/");
    if path == base {
        return String::new();
    }
    let needle = format!("{base}/");
    path.strip_prefix(&needle).unwrap_or(&path).to_string()
}

fn join_path(root: &str, suffix: &str) -> String {
    if root.is_empty() {
        return suffix.to_string();
    }
    if root.ends_with('/') || root.ends_with('\\') {
        format!("{root}{suffix}")
    } else {
        format!("{root}/{suffix}")
    }
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once(['/', '\\'])
        .map(|(parent, _)| parent.to_string())
        .unwrap_or_default()
}

fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

fn sanitize_id(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return "gradle-workspace".to_string();
    }
    trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn quote_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("\"{}\"", escape_json(item)))
        .collect::<Vec<_>>()
        .join(",")
}

fn opt_json(value: &Option<String>) -> String {
    match value {
        Some(value) => format!("\"{}\"", escape_json(value)),
        None => "null".to_string(),
    }
}

fn escape_json(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r#"
pluginManagement { repositories { google(); mavenCentral() } }
rootProject.name = "ADD Helper"
include(":app")
include(":feature:calendar")
"#;

    const APP_BUILD: &str = r#"
plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}
android {
    namespace = "com.adevcorn.addhelper"
    compileSdk = 34
    defaultConfig {
        applicationId = "com.adevcorn.addhelper"
        minSdk = 31
    }
    buildFeatures {
        compose = true
        aidl = true
    }
}
"#;

    const LIB_BUILD: &str = r#"
plugins {
    id("com.android.library")
    id("org.jetbrains.kotlin.android")
}
android { namespace = "com.adevcorn.feature.calendar" }
"#;

    #[test]
    fn parses_settings_root_name_and_includes() {
        let (name, includes) = parse_settings(SETTINGS);
        assert_eq!(name.as_deref(), Some("ADD Helper"));
        assert_eq!(includes, vec!["app", "feature/calendar"]);
    }

    #[test]
    fn parses_gradle_kotlin_dsl_strings() {
        assert_eq!(
            parse_gradle_string(APP_BUILD, "namespace").as_deref(),
            Some("com.adevcorn.addhelper")
        );
        assert_eq!(
            parse_gradle_string(APP_BUILD, "applicationId").as_deref(),
            Some("com.adevcorn.addhelper")
        );
    }

    #[test]
    fn classifies_android_modules() {
        let app = parse_module(
            "app",
            APP_BUILD,
            Some("<manifest><service/></manifest>"),
            true,
            true,
        );
        assert_eq!(app.kind, "app");
        assert_eq!(app.subtype, "android-app");
        assert!(app.capabilities.contains(&"has-aidl".to_string()));
        assert!(app.capabilities.contains(&"has-services".to_string()));
        assert!(app.capabilities.contains(&"uses-compose".to_string()));
        assert_eq!(app.namespace.as_deref(), Some("com.adevcorn.addhelper"));

        let lib = parse_module("feature/calendar", LIB_BUILD, None, false, true);
        assert_eq!(lib.kind, "library");
        assert_eq!(lib.subtype, "android-library");
        assert_eq!(lib.name, "calendar");
    }

    #[test]
    fn model_contains_modules_and_owners() {
        let modules = vec![
            parse_module(
                "android/app",
                APP_BUILD,
                Some("<manifest><activity/></manifest>"),
                true,
                true,
            ),
            parse_module("android/feature/calendar", LIB_BUILD, None, false, true),
        ];
        let json = emit_model(
            "/ws",
            "android/settings.gradle.kts",
            "ADD Helper",
            &modules,
            "android",
        );
        assert!(json.contains("\"ecosystem\":\"gradle-android\""));
        assert!(json.contains("\"subtype\":\"gradle-workspace\""));
        assert!(json.contains("\"subtype\":\"android-app\""));
        assert!(json.contains("\"subtype\":\"android-library\""));
        assert!(json.contains("\"path\":\"android/app/src/main/\",\"match_kind\":\"prefix\""));
        assert!(json
            .contains("\"path\":\"android/feature/calendar/src/test/\",\"match_kind\":\"prefix\""));
        assert!(json.contains("\"path\":\"android/settings.gradle.kts\",\"match_kind\":\"exact\""));
    }

    #[test]
    fn review_actions_cover_build_test_and_lint() {
        let ws = temp_workspace(&[(
            "settings.gradle.kts",
            "rootProject.name = \"demo\"\ninclude(\":app\")\n",
        )]);
        let actions = review_actions(ws.to_str().unwrap(), ws.to_str().unwrap());
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["gradle-build", "gradle-test", "gradle-lint"]);
        assert_eq!(actions[0].command_preview, "gradle compileDebugSources");
        assert_eq!(actions[1].kind, ReviewActionKind::Test);
        assert_eq!(actions[2].kind, ReviewActionKind::Lint);
        assert!(review_actions("/definitely/not/a/gradle/project", "/tmp").is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn temp_workspace(files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gradle-model-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, contents) in files {
            std::fs::write(dir.join(name), contents).unwrap();
        }
        dir
    }

    #[test]
    fn review_plans_fall_back_to_gradle_without_wrapper() {
        let plan = review_action_plan("gradle-test", "/ws", "/ws").unwrap();
        assert_eq!(plan.executable, "gradle");
        assert_eq!(plan.args, vec!["testDebugUnitTest", "--console=plain"]);
        assert_eq!(plan.cwd_mode, ReviewActionCwdMode::SessionWorkspace);
    }

    #[test]
    fn gradle_build_plan_specifies_compile_debug_sources_instead_of_assemble_debug() {
        let plan = review_action_plan("gradle-build", "/ws", "/ws").unwrap();
        assert_eq!(plan.executable, "gradle");
        assert_eq!(plan.args, vec!["compileDebugSources", "--console=plain"]);
        assert!(!plan.args.contains(&"assembleDebug".to_string()));
        assert_eq!(plan.output_category, "build");
    }

    #[test]
    fn kotlin_compiler_errors_become_entries() {
        let line = b"e: file:///C:/ws/app/src/main/java/Foo.kt:12:34 Unresolved reference: bar";
        let (state, events) = review_action_parse_line("gradle-build", line, &[]);
        assert_eq!(read_counter(&state), 1);
        match &events[0] {
            AgentEvent::NewEntry {
                category,
                raw_cmd,
                file_paths,
                ..
            } => {
                assert_eq!(category, "build");
                assert!(raw_cmd.contains("Unresolved reference"));
                assert!(raw_cmd.contains("\"line\":12"));
                assert!(raw_cmd.contains("C:/ws/app/src/main/java/Foo.kt"));
                assert!(file_paths.is_empty());
            }
            _ => panic!("expected NewEntry"),
        }
    }

    #[test]
    fn gradle_test_summary_sets_exit_code() {
        let (_, events) = review_action_parse_line(
            "gradle-test",
            b"Tests run: 3, Failures: 1, Errors: 0, Skipped: 0",
            &[],
        );
        assert!(matches!(
            &events[0],
            AgentEvent::CloseEntry { exit_code: 1, .. }
        ));

        let (_, ok) = review_action_parse_line(
            "gradle-test",
            b"Tests run: 3, Failures: 0, Errors: 0, Skipped: 0",
            &[],
        );
        assert!(matches!(
            &ok[0],
            AgentEvent::CloseEntry { exit_code: 0, .. }
        ));
    }

    #[test]
    fn failure_banner_is_reported() {
        let (_, events) = review_action_parse_line(
            "gradle-build",
            b"FAILURE: Build failed with an exception.",
            &[],
        );
        assert!(matches!(
            &events[0],
            AgentEvent::NewEntry { category, .. } if category == "build"
        ));
    }

    #[test]
    fn ansi_sequences_are_stripped() {
        let raw = b"\x1b[31me: file:///C:/Foo.kt:1:2 bad\x1b[0m";
        let (_, events) = review_action_parse_line("gradle-build", raw, &[]);
        assert!(matches!(&events[0], AgentEvent::NewEntry { .. }));
    }

    #[test]
    fn non_failing_gradle_tasks_do_not_emit_entries() {
        let non_failing = [
            b"Task :app:preBuild UP-TO-DATE".as_slice(),
            b"Task :app:compileDebugAidl UP-TO-DATE",
            b"Task :app:compileDebugKotlin",
            b":app:assembleDebug UP-TO-DATE",
            b":app:assembleDebug",
            b":app:compileDebugSources UP-TO-DATE",
            b":app:compileDebugSources",
            b"> Task :app:preBuild UP-TO-DATE",
            b"> Task :app:compileDebugKotlin",
            b"Task :app:compileDebugKotlin FROM-CACHE",
            b"Task :app:compileDebugKotlin SKIPPED",
            b"Task :app:compileDebugKotlin NO-SOURCE",
        ];
        for line in non_failing {
            let (_, events) = review_action_parse_line("gradle-build", line, &[]);
            assert!(
                events.is_empty(),
                "expected zero events for line: {}",
                String::from_utf8_lossy(line)
            );
        }

        let (_, failed_task) = review_action_parse_line(
            "gradle-build",
            b"Task :app:compileDebugKotlin FAILED",
            &[],
        );
        assert_eq!(failed_task.len(), 1);
        assert!(matches!(
            &failed_task[0],
            AgentEvent::NewEntry { category, raw_cmd, .. }
                if category == "build" && raw_cmd == "Task :app:compileDebugKotlin FAILED"
        ));

        let (_, arrow_failed_task) = review_action_parse_line(
            "gradle-build",
            b"> Task :app:compileDebugKotlin FAILED",
            &[],
        );
        assert_eq!(arrow_failed_task.len(), 1);
        assert!(matches!(
            &arrow_failed_task[0],
            AgentEvent::NewEntry { category, raw_cmd, .. }
                if category == "build" && raw_cmd == "Task :app:compileDebugKotlin FAILED"
        ));

        let (_, colon_failed_task) = review_action_parse_line(
            "gradle-build",
            b":app:compileDebugKotlin FAILED",
            &[],
        );
        assert_eq!(colon_failed_task.len(), 1);
        assert!(matches!(
            &colon_failed_task[0],
            AgentEvent::NewEntry { category, raw_cmd, .. }
                if category == "build" && raw_cmd == "Task :app:compileDebugKotlin FAILED"
        ));
    }

    /// Opt-in end-to-end model build against the real add-helper checkout.
    #[test]
    #[ignore = "requires ../../add-helper"]
    fn models_real_add_helper_android_project() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../add-helper");
        let bytes = build_project_model(root.to_str().unwrap());
        let json = String::from_utf8(bytes).expect("utf8 model");
        assert!(json.contains("\"ecosystem\":\"gradle-android\""), "{json}");
        assert!(json.contains("\"display_name\":\"ADD Helper\""), "{json}");
        assert!(json.contains("\"subtype\":\"android-app\""), "{json}");
        assert!(json.contains("\"language\":\"kotlin\""), "{json}");
        assert!(
            json.contains("\"path\":\"android/app/src/main/\",\"match_kind\":\"prefix\""),
            "{json}"
        );
        assert!(
            json.contains("\"path\":\"android/settings.gradle.kts\",\"match_kind\":\"exact\""),
            "{json}"
        );
        assert!(json.contains("\"has-aidl\""), "{json}");
        assert!(json.contains("\"uses-compose\""), "{json}");
    }
}
