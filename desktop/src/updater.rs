use crate::backend::Reply;
use anyhow::{Context, Result};
use herdr_feishu_bridge::update::{self, Preferences, Status};
use std::{path::PathBuf, sync::mpsc, time::Duration};

pub enum Action {
    Check,
    Install,
    PreferencesChanged,
    Restart,
    Rollback,
}
pub fn start(data: PathBuf, replies: mpsc::Sender<Reply>) -> mpsc::Sender<Action> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let client = update::client()?;
            let app = update::bundle();
            let mut status = Status::new(Preferences::load(&data)?);
            status.rollback_available = data.join("updates/installed.json").is_file();
            let send = |status: &Status| {
                let _ = status.save(&data);
                let _ = replies.send(Reply::Update(
                    serde_json::to_value(status).unwrap_or_default(),
                ));
            };
            send(&status);
            let mut latest = None;
            let mut next_check = std::time::Instant::now() + Duration::from_secs(6 * 60 * 60);
            let mut action = status
                .preferences
                .automatic
                .then_some((Action::Check, false));
            loop {
                if let Some((action, manual)) = action.take() {
                    let result = runtime.block_on(async {
                        match action {
                            Action::PreferencesChanged => {
                                status.preferences = Preferences::load(&data)?;
                                latest = None;
                                if status.phase != "installed" {
                                    status.phase = "idle".into();
                                    status.latest = None;
                                    status.notes.clear();
                                    status.progress = None;
                                }
                                status.message = if status.preferences.automatic {
                                    "Automatic updates enabled: checks every six hours".into()
                                } else {
                                    "Automatic updates disabled. Manual checks remain available.".into()
                                };
                                send(&status);
                            }
                            Action::Restart => {
                                let app = app.as_ref().context("This executable is not inside an installed macOS app")?;
                                update::relaunch_after_exit(app, &data, std::process::id())?;
                                let _ = replies.send(Reply::Restart);
                                return Ok(());
                            }
                            Action::Rollback => {
                                update::rollback_and_restart(&data).await?;
                                status.preferences = Preferences::load(&data)?;
                                status.phase = "installed".into();
                                status.message =
                                    "Rolled back and restarted the daemon. Reopen the desktop to apply; automatic updates are paused.".into();
                                status.latest = None;
                                status.rollback_available = false;
                                send(&status);
                            }
                            Action::Check | Action::Install => {
                                next_check =
                                    std::time::Instant::now() + Duration::from_secs(6 * 60 * 60);
                                let install = matches!(action, Action::Install);
                                if !install || latest.is_none() {
                                    status.phase = "checking".into();
                                    status.message = "Checking GitHub releases...".into();
                                    status.progress = None;
                                    send(&status);
                                    status.preferences = Preferences::load(&data)?;
                                    latest = update::check(&client, &status.preferences).await?;
                                }
                                if let Some(release) = &latest {
                                    status.latest = Some(release.version().into());
                                    status.notes = release.notes.clone();
                                    status.release_url = release.page.clone();
                                    status.phase = "available".into();
                                    status.message = format!("New version available: {}", release.version());
                                    send(&status);
                                    if app
                                        .as_ref()
                                        .and_then(|p| update::bundle_version(p).ok())
                                        .as_deref()
                                        == Some(release.version())
                                    {
                                        status.phase = "installed".into();
                                        status.message =
                                            format!("{} installed. Restart the desktop to apply.", release.version());
                                        send(&status);
                                        return Ok(());
                                    }
                                    // An installed app can update itself; source/ad-hoc builds only check.
                                    let trusted = app.as_ref().is_some_and(|app| {
                                        update::verify_bundle(app, None).is_ok()
                                    });
                                    if install
                                        || (!manual && status.preferences.automatic && trusted)
                                    {
                                        let app = app
                                            .as_ref()
                                            .context("Install a signed Dauerbot app before using automatic updates")?;
                                        status.phase = "downloading".into();
                                        status.message = "Downloading and verifying the update. Your project keeps running.".into();
                                        send(&status);
                                        update::install(
                                            &client,
                                            &data,
                                            app,
                                            release,
                                            |percent| {
                                                let mut progress = status.clone();
                                                progress.progress = Some(percent);
                                                progress.message = if percent == 100 {
                                                    "Download complete. Verifying signatures and replacing the app...".into()
                                                } else {
                                                    format!("Downloading update: {percent}%")
                                                };
                                                let _ = replies.send(Reply::Update(
                                                    serde_json::to_value(progress)
                                                        .unwrap_or_default(),
                                                ));
                                            },
                                            !manual,
                                        )
                                        .await?;
                                        status.phase = "installed".into();
                                        status.rollback_available = true;
                                        status.progress = None;
                                        status.message = format!(
                                            "{} installed. Restart to apply; the current main session keeps running.",
                                            release.version()
                                        );
                                        send(&status);
                                    }
                                } else {
                                    status.phase = "current".into();
                                    status.latest = None;
                                    status.message = "You have the latest version on this channel".into();
                                    send(&status);
                                }
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    });
                    if let Err(error) = result {
                        status.preferences = Preferences::load(&data)?;
                        status.phase = "error".into();
                        status.progress = None;
                        status.message = format!("{error:#}");
                        send(&status);
                    }
                }
                action = match rx.recv_timeout(Duration::from_secs(30)) {
                    Ok(action) => Some((action, true)),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let preferences = Preferences::load(&data)?;
                        if preferences != status.preferences {
                            Some((Action::PreferencesChanged, true))
                        } else if preferences.automatic && std::time::Instant::now() >= next_check {
                            Some((Action::Check, false))
                        } else {
                            None
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
            }
            Ok(())
        })();
        if let Err(error) = result {
            let mut status = Status::new(Preferences::default());
            status.phase = "error".into();
            status.message = format!("Update service failed to start: {error:#}");
            let _ = replies.send(Reply::Update(
                serde_json::to_value(status).unwrap_or_default(),
            ));
        }
    });
    tx
}
