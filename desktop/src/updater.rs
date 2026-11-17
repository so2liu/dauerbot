use crate::backend::Reply;
use anyhow::{Context, Result};
use herdr_feishu_bridge::update::{self, Preferences, Status};
use std::{path::PathBuf, sync::mpsc, time::Duration};

pub enum Action {
    Check,
    Install,
    PreferencesChanged,
}
pub fn start(
    data: PathBuf,
    replies: mpsc::Sender<Reply>,
    gate: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
) -> mpsc::Sender<Action> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if let Some(gate) = gate {
            while !gate.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(25));
            }
        }
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let client = update::client()?;
            let app = update::bundle();
            let mut recovery_pending = runtime.block_on(recover_activation(&data)).is_err();
            let mut status = Status::new(Preferences::load(&data)?);
            status.rollback_available = data.join("updates/installed.json").is_file();
            apply_receipt_status(&data, &mut status)?;
            let send = |status: &Status| {
                let _ = status.save(&data);
                let _ = replies.send(Reply::Update(
                    serde_json::to_value(status).unwrap_or_default(),
                ));
            };
            if recovery_pending {
                status.phase = "error".into();
                status.message =
                    "Waiting to confirm the update. Your projects are preserved.".into();
            }
            send(&status);
            let mut latest = None;
            let mut next_check = std::time::Instant::now() + Duration::from_secs(6 * 60 * 60);
            let mut action = (status.preferences.automatic && !recovery_pending)
                .then_some((Action::Check, false));
            loop {
                if let Some((action, manual)) = action.take() {
                    let result = runtime.block_on(async {
                        if recovery_pending {
                            recover_activation(&data).await?;
                            recovery_pending = false;
                        }
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
                                apply_receipt_status(&data,&mut status)?;
                                send(&status);
                            }
                            Action::Check | Action::Install => {
                                if manual { recover_activation(&data).await?; }
                                if let Some(journal) = update::activation::load(&data)?
                                    && journal.owner_committed && matches!(journal.phase.as_str(),"committed"|"publication_pending") {
                                    anyhow::ensure!(manual,"The updated app could not open. Your projects keep running; try the update again.");
                                    let mut transaction = update::activation::Transaction::recover(&data)?.context("Missing update receipt")?;
                                    transaction.retry_publication()?;
                                    status.phase = "activating".into();
                                    status.message = "Finishing the update. Your projects keep running.".into();
                                    send(&status);
                                    crate::activation::activate(transaction,&data,&replies).await?;
                                    return Ok(());
                                }

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
                                        anyhow::bail!("The app files are prepared, but activation has not been confirmed. Your projects keep running.");
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
                                        let transaction = update::prepare_install(
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
                                        status.phase = "activating".into();
                                        status.progress = None;
                                        status.message = "Finishing the update while your projects keep running...".into();
                                        send(&status);
                                        crate::activation::activate(transaction, &data, &replies).await?;
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
                        if recovery_pending {
                            match runtime.block_on(recover_activation(&data)) {
                                Ok(()) => {
                                    recovery_pending = false;
                                    Some((Action::PreferencesChanged, false))
                                }
                                Err(_) => None,
                            }
                        } else if preferences != status.preferences {
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

fn apply_receipt_status(data: &std::path::Path, status: &mut Status) -> Result<()> {
    if let Some(journal) = update::activation::load(data)?
        && journal.phase != "complete"
    {
        status.latest = Some(journal.version);
        status.progress = None;
        status.phase = if journal.error.is_some() || journal.phase == "failed" {
            "error"
        } else {
            "activating"
        }
        .into();
        status.message = journal.error.unwrap_or_else(|| {
            "Waiting to confirm the update. Your projects are preserved.".into()
        });
    }
    Ok(())
}

async fn recover_activation(data: &std::path::Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let Some(journal) = update::activation::load(data)? else {
        return Ok(());
    };
    if matches!(journal.phase.as_str(), "complete" | "failed") {
        return Ok(());
    }
    let cfg: herdr_feishu_bridge::config::Config =
        serde_json::from_slice(&std::fs::read(data.join("config.json"))?)?;
    let state = crate::backend::request(&client, &cfg, "GET", "/state", None).await?;
    let transaction = update::activation::Transaction::recover(data)?
        .context("The activation receipt disappeared")?;
    if journal.version == update::CURRENT && crate::backend::service_healthy(&state) {
        let mut transaction = transaction;
        transaction.committed()?;
        transaction.finish()?;
        return Ok(());
    }
    if journal.owner_committed {
        return Ok(());
    }
    if matches!(journal.phase.as_str(), "preparing" | "installed") {
        transaction.rollback("Update preparation was interrupted before activation")?;
        return Ok(());
    }
    let receipt = crate::backend::request(
        &client,
        &cfg,
        "POST",
        "/service/activation/abort",
        Some(serde_json::json!({"activation_id":journal.id})),
    )
    .await?;
    if receipt["owner_committed"] == true {
        let mut transaction = transaction;
        transaction.committed()?;
    } else if matches!(receipt["phase"].as_str(), Some("failed" | "aborted")) {
        transaction.rollback("Update activation was interrupted before ownership changed")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        fs,
        io::{Read, Write},
        net::TcpListener,
    };
    #[test]
    fn delayed_service_does_not_kill_the_recovery_worker_or_lose_ownership() {
        let data = tempfile::tempdir().unwrap();
        fs::create_dir(data.path().join("updates")).unwrap();
        Preferences {
            automatic: false,
            channel: "stable".into(),
        }
        .save(data.path())
        .unwrap();
        let reservation = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let cfg = herdr_feishu_bridge::config::Config {
            port: reservation.local_addr().unwrap().port(),
            ..Default::default()
        };
        drop(reservation);
        fs::write(
            data.path().join("config.json"),
            serde_json::to_vec(&cfg).unwrap(),
        )
        .unwrap();
        fs::write(data.path().join("updates/activation.json"),json!({"id":"delayed-service","version":update::CURRENT,
            "app":data.path().join("Dauerbot.app"),"backup":data.path().join(".dauerbot-update-fixture/old"),
            "phase":"committed","owner_committed":true,"previous_record":null,"error":null,
            "original_identity":[0,0],"gui_attempts":1,"gui_pid":null}).to_string()).unwrap();
        let (reply, receiver) = mpsc::channel();
        let actions = start(data.path().to_owned(), reply, None);
        let initial = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(initial,Reply::Update(value) if value["phase"]=="error"));
        let server = TcpListener::bind(("127.0.0.1", cfg.port)).unwrap();
        let fixture = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let state =
                json!({"version":update::CURRENT,"api_protocol":herdr_feishu_bridge::api::PROTOCOL,
                "service_activation":{"capability":1},"mcp_contracts":[1],"projects":[]})
                .to_string();
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{state}",state.len()).unwrap();
        });
        actions.send(Action::PreferencesChanged).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if update::activation::load(data.path())
                .unwrap()
                .unwrap()
                .phase
                == "complete"
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture.join().unwrap();
        drop(actions);
    }
}
