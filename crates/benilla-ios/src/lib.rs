//! The iOS launcher: Xcode links this static library into the app (`ios/project.yml`), and the
//! app's `ios/main.m` calls [`main_rs`], which hands the build stamp to `benilla-app` as the
//! desktop `benilla` binary does. winit runs `UIApplicationMain` on the main thread from inside
//! `run` and never returns.

use benilla_app::BuildId;

/// The app's entry, called from `ios/main.m`.
#[no_mangle]
pub extern "C" fn main_rs() {
    #[cfg(any(
        feature = "route-probe",
        feature = "crowd-probe",
        feature = "chat-probe",
        feature = "quest-probe"
    ))]
    load_launch_env();
    let _ = benilla_app::run(BuildId {
        version: env!("CARGO_PKG_VERSION"),
        describe: env!("BENILLA_GIT_DESCRIBE"),
        sha: env!("BENILLA_GIT_SHA"),
        short: env!("BENILLA_GIT_SHORT"),
        date: env!("BENILLA_GIT_DATE"),
        profile: env!("BENILLA_PROFILE"),
        project_dir: env!("BENILLA_PROJECT_DIR"),
        ..Default::default()
    });
}

/// A probe build's launch environment from `Documents/benilla-config/launch.env` (one `KEY=VALUE`
/// a line, the value taken whole after the first `=`), set before anything reads the environment
/// and the file deleted, so it applies to this one launch. The test tools write it over AFC
/// (`wow-optimize/tools/ipad_run.sh`): the debugger's launch environment (`idevicedebug -e`)
/// intermittently never reached the app (2026-10-31, and twice on 2026-11-10: a fresh process with
/// none of the variables, so no meter, sound on and no login). Main thread, before `run` starts
/// any other, so setting variables races nothing.
#[cfg(any(
    feature = "route-probe",
    feature = "crowd-probe",
    feature = "chat-probe",
    feature = "quest-probe"
))]
fn load_launch_env() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = std::path::Path::new(&home).join("Documents/benilla-config/launch.env");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if !key.is_empty() {
                std::env::set_var(key, value);
            }
        }
    }
}
