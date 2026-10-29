//! The iOS launcher: Xcode links this static library into the app (`ios/project.yml`), and the
//! app's `ios/main.m` calls [`main_rs`], which hands the build stamp to `benilla-app` as the
//! desktop `benilla` binary does. winit runs `UIApplicationMain` on the main thread from inside
//! `run` and never returns.

use benilla_app::BuildId;

/// The app's entry, called from `ios/main.m`.
#[no_mangle]
pub extern "C" fn main_rs() {
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
