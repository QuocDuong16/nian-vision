#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use nian_domain::{
    AudioPolicy, CameraConfig, CameraEndpoint, CameraId, CameraSource, CredentialRef, Host,
};
use nian_settings::{ApplicationSettings, SettingsStore};
use serde_json::json;

const CAMERA_ID: &str = "release-fixture-camera";
const CREDENTIAL_REF: &str = "nian-vision/release-fixture/credential-ref";

fn camera() -> Result<CameraConfig, String> {
    CameraConfig::new(
        CameraId::parse(CAMERA_ID).map_err(|error| error.to_string())?,
        "Release fixture camera",
        CameraSource::Rtsp(
            CameraEndpoint::new(
                Host::parse("192.0.2.10").map_err(|error| error.to_string())?,
                554,
                "/stream1",
            )
            .map_err(|error| error.to_string())?,
        ),
        AudioPolicy::CopyAll,
        CredentialRef::parse(CREDENTIAL_REF).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn create(path: &Path, footage_root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(footage_root).map_err(|error| error.to_string())?;
    let marker = footage_root.join("preserve-me.mkv");
    std::fs::write(&marker, b"release-fixture-footage").map_err(|error| error.to_string())?;

    let mut store = SettingsStore::open(path).map_err(|error| error.to_string())?;
    let camera = camera()?;
    store
        .insert_camera(&camera)
        .map_err(|error| error.to_string())?;
    let settings = ApplicationSettings {
        storage_root: Some(footage_root.to_path_buf()),
        launch_at_login: true,
        ..ApplicationSettings::default()
    };
    store
        .save_application_settings(&settings)
        .map_err(|error| error.to_string())?;
    if !store
        .set_recording_enabled(camera.camera_id(), true)
        .map_err(|error| error.to_string())?
    {
        return Err(
            "release fixture camera disappeared while setting desired recording".to_owned(),
        );
    }
    Ok(())
}

fn verify(path: &Path, footage_root: &Path) -> Result<(), String> {
    let store = SettingsStore::open(path).map_err(|error| error.to_string())?;
    let camera_id = CameraId::parse(CAMERA_ID).map_err(|error| error.to_string())?;
    let camera = store
        .get_camera(&camera_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "release fixture camera missing after upgrade".to_owned())?;
    if camera.credential_ref().as_str() != CREDENTIAL_REF {
        return Err("credential ref changed across installed upgrade".to_owned());
    }
    let desired = store
        .recording_enabled_cameras()
        .map_err(|error| error.to_string())?;
    if desired != vec![camera_id] {
        return Err("recording_enabled desired intent changed across installed upgrade".to_owned());
    }
    let settings = store
        .application_settings()
        .map_err(|error| error.to_string())?;
    if !settings.launch_at_login {
        return Err("launch_at_login preference changed across installed upgrade".to_owned());
    }
    if settings.storage_root.as_deref() != Some(footage_root) {
        return Err("recording storage path changed across installed upgrade".to_owned());
    }
    let marker = footage_root.join("preserve-me.mkv");
    if std::fs::read(&marker).map_err(|error| error.to_string())? != b"release-fixture-footage" {
        return Err("recording footage marker changed across installed upgrade".to_owned());
    }
    println!(
        "{}",
        json!({"settings_preserved": true, "footage_preserved": true})
    );
    Ok(())
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let command = args.next().and_then(|value| value.into_string().ok());
    let path = args.next().map(PathBuf::from);
    let footage = args.next().map(PathBuf::from);
    let result = match (command.as_deref(), path.as_deref(), footage.as_deref()) {
        (Some("create"), Some(path), Some(footage)) => create(path, footage),
        (Some("verify"), Some(path), Some(footage)) => verify(path, footage),
        _ => Err(
            "usage: nian-settings-fixture <create|verify> <settings.sqlite3> <footage-root>"
                .to_owned(),
        ),
    };
    if let Err(error) = result {
        eprintln!("nian-settings-fixture: {error}");
        std::process::exit(1);
    }
}
