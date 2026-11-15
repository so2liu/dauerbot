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
                                    "自动升级已开启：每 6 小时检查".into()
                                } else {
                                    "自动升级已关闭，仍可手动检查".into()
                                };
                                send(&status);
                            }
                            Action::Restart => {
                                let app = app.as_ref().context("当前不是已安装的 macOS 应用")?;
                                update::relaunch_after_exit(app, &data, std::process::id())?;
                                let _ = replies.send(Reply::Restart);
                                return Ok(());
                            }
                            Action::Rollback => {
                                update::rollback_and_restart(&data).await?;
                                status.preferences = Preferences::load(&data)?;
                                status.phase = "installed".into();
                                status.message =
                                    "已回退并重启后台，桌面重开后生效；自动升级已暂停".into();
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
                                    status.message = "正在检查 GitHub 发布…".into();
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
                                    status.message = format!("发现新版本 {}", release.version());
                                    send(&status);
                                    if app
                                        .as_ref()
                                        .and_then(|p| update::bundle_version(p).ok())
                                        .as_deref()
                                        == Some(release.version())
                                    {
                                        status.phase = "installed".into();
                                        status.message =
                                            format!("{} 已安装，重启桌面后生效", release.version());
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
                                            .context("请先安装签名版 Dauerbot，再使用自动升级")?;
                                        status.phase = "downloading".into();
                                        status.message = "正在下载并校验更新，项目继续运行".into();
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
                                                    "下载完成，正在校验签名并替换应用…".into()
                                                } else {
                                                    format!("正在下载更新 {percent}%")
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
                                            "{} 已安装，重启后生效；当前主会话继续运行",
                                            release.version()
                                        );
                                        send(&status);
                                    }
                                } else {
                                    status.phase = "current".into();
                                    status.latest = None;
                                    status.message = "当前已是此通道的最新版本".into();
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
            status.message = format!("更新服务未启动：{error:#}");
            let _ = replies.send(Reply::Update(
                serde_json::to_value(status).unwrap_or_default(),
            ));
        }
    });
    tx
}
