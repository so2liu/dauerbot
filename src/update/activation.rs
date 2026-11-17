//! One durable receipt spans bundle preparation, service ownership and GUI publication.
use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Journal {
    pub id: String,
    pub version: String,
    pub app: PathBuf,
    pub backup: PathBuf,
    pub phase: String,
    pub owner_committed: bool,
    pub previous_record: Option<Vec<u8>>,
    pub error: Option<String>,
    pub original_identity: [u64; 2],
    #[serde(default)]
    pub gui_attempts: u32,
    #[serde(default)]
    pub gui_pid: Option<u32>,
}

pub struct Transaction {
    data: PathBuf,
    pub journal: Journal,
    _lock: File,
}

pub fn lock(data: &Path) -> Result<File> {
    config::private_dir(&data.join("updates"))?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("updates/update.lock"))?;
    file.try_lock_exclusive()
        .context("Another update is in progress")?;
    Ok(file)
}

pub fn ensure_not_suppressed(data: &Path, version: &str, automatic: bool) -> Result<()> {
    if let Some(journal) = load(data)? {
        ensure!(
            matches!(journal.phase.as_str(), "failed" | "complete"),
            "An update is still being applied. Your projects keep running."
        );
        ensure!(
            !automatic || journal.version != version || journal.phase != "failed",
            "This update could not be activated. The current app keeps running; check manually to try again."
        );
    }
    Ok(())
}

pub fn load(data: &Path) -> Result<Option<Journal>> {
    let path = data.join("updates/activation.json");
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).context(
            "The update receipt cannot be read; preserving the installed app",
        )?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

// The explicit CLI rollback already holds the update lock and validates its
// signed backup. Keep the activation receipt consistent with that completed swap.
pub(super) fn legacy_rollback_completed(data: &Path, app: &Path) -> Result<()> {
    if let Some(mut journal) = load(data)? {
        ensure!(
            !journal.owner_committed,
            "Committed ownership cannot be rolled back"
        );
        if journal.app == app && !matches!(journal.phase.as_str(), "complete" | "failed") {
            restore_record(
                &data.join("updates/installed.json"),
                journal.previous_record.as_deref(),
            )?;
            journal.phase = "failed".into();
            journal.error = Some("The update was rolled back".into());
            config::write_private(
                &data.join("updates/activation.json"),
                &serde_json::to_vec(&journal)?,
            )?;
            File::open(data.join("updates"))?.sync_all()?;
        }
    }
    Ok(())
}

impl Transaction {
    pub(super) fn prepare(
        data: &Path,
        app: &Path,
        backup: &Path,
        version: &str,
        previous_record: Option<Vec<u8>>,
        lock: File,
    ) -> Result<Self> {
        let transaction = Self {
            data: data.to_owned(),
            _lock: lock,
            journal: Journal {
                id: uuid::Uuid::new_v4().to_string(),
                version: version.into(),
                app: app.to_owned(),
                backup: backup.to_owned(),
                phase: "preparing".into(),
                owner_committed: false,
                previous_record,
                error: None,
                original_identity: identity(app)?,
                gui_attempts: 0,
                gui_pid: None,
            },
        };
        transaction.save()?;
        Ok(transaction)
    }
    pub fn recover(data: &Path) -> Result<Option<Self>> {
        let lock = lock(data)?;
        Ok(load(data)?.map(|journal| Self {
            data: data.to_owned(),
            journal,
            _lock: lock,
        }))
    }
    fn save(&self) -> Result<()> {
        config::write_private(
            &self.data.join("updates/activation.json"),
            &serde_json::to_vec(&self.journal)?,
        )?;
        File::open(self.data.join("updates"))?.sync_all()?;
        Ok(())
    }
    pub(super) fn prepared(&mut self) -> Result<()> {
        self.journal.phase = "prepared".into();
        self.save()
    }
    pub fn begin_gui(&mut self) -> Result<()> {
        ensure!(
            self.journal.gui_attempts < 3,
            "The updated app could not open. Your projects keep running; a later update can try again."
        );
        self.journal.gui_attempts += 1;
        self.save()
    }
    pub fn gui_started(&mut self, pid: u32) -> Result<()> {
        self.journal.gui_pid = Some(pid);
        self.save()
    }
    pub fn retry_publication(&mut self) -> Result<()> {
        ensure!(
            self.journal.owner_committed,
            "Only committed publication can be retried"
        );
        self.journal.gui_attempts = 0;
        self.save()
    }
    pub fn publication_pending(&mut self, reason: &str) -> Result<()> {
        ensure!(
            self.journal.owner_committed,
            "Publication recovery requires committed ownership"
        );
        self.journal.phase = "publication_pending".into();
        self.journal.error = Some(reason.into());
        self.save()
    }
    pub fn committed(&mut self) -> Result<()> {
        self.journal.owner_committed = true;
        self.journal.phase = "committed".into();
        self.save()
    }
    pub fn finish(mut self) -> Result<()> {
        ensure!(
            self.journal.owner_committed,
            "Ownership must be confirmed before finishing activation"
        );
        self.journal.phase = "complete".into();
        self.save()?;
        let previous = self
            .journal
            .previous_record
            .as_ref()
            .map(|bytes| serde_json::from_slice::<InstallRecord>(bytes))
            .transpose()?;
        cleanup_previous(previous, &self.journal.app);
        Ok(())
    }
    /// CLI installation deliberately leaves activation for the next desktop launch.
    pub(super) fn finish_install_only(mut self) -> Result<()> {
        self.journal.phase = "installed".into();
        self.save()
    }
    pub fn rollback(mut self, reason: &str) -> Result<()> {
        ensure!(
            !self.journal.owner_committed,
            "Committed ownership cannot restore an old app or database"
        );
        let stage = self
            .journal
            .backup
            .parent()
            .context("Missing update stage")?;
        ensure!(
            stage.parent() == self.journal.app.parent()
                && stage
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".dauerbot-update-"))
                && self
                    .journal
                    .backup
                    .file_name()
                    .is_some_and(|name| name == "old"),
            "Invalid update receipt paths"
        );
        if self.journal.backup.is_dir() {
            ensure!(
                identity(&self.journal.backup)? == self.journal.original_identity,
                "The preserved app no longer matches the original; keeping the update receipt"
            );
            if self.journal.app.exists() {
                fs::rename(&self.journal.app, stage.join("failed"))?;
            }
            fs::rename(&self.journal.backup, &self.journal.app)?;
            File::open(self.journal.app.parent().context("Invalid app path")?)?.sync_all()?;
        } else {
            ensure!(
                identity(&self.journal.app).ok() == Some(self.journal.original_identity),
                "The original app backup is missing; keeping the installed app and its recovery receipt"
            );
        }
        restore_record(
            &self.data.join("updates/installed.json"),
            self.journal.previous_record.as_deref(),
        )?;
        self.journal.phase = "failed".into();
        self.journal.error = Some(reason.into());
        self.save()?;
        // The original is back at its canonical path. This transaction's failed
        // payload is no longer executable or a rollback root.
        let _ = fs::remove_dir_all(stage);
        Ok(())
    }
}

fn identity(path: &Path) -> Result<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "The original app is not a regular bundle"
    );
    Ok([metadata.dev(), metadata.ino()])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepared(root: &Path) -> Transaction {
        let data = root.join("data");
        let app = root.join("Dauerbot.app");
        let backup = root.join(".dauerbot-update-test/old");
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(backup.parent().unwrap()).unwrap();
        fs::write(app.join("version"), b"old").unwrap();
        let lock = lock(&data).unwrap();
        config::write_private(&data.join("updates/installed.json"), b"new record").unwrap();
        let transaction = Transaction::prepare(
            &data,
            &app,
            &backup,
            "9.0.0",
            Some(b"old record".to_vec()),
            lock,
        )
        .unwrap();
        fs::rename(&app, &backup).unwrap();
        fs::create_dir(&app).unwrap();
        fs::write(app.join("version"), b"new").unwrap();
        transaction
    }
    #[test]
    fn failed_activation_restores_bundle_record_and_suppresses_only_automatic_retry() {
        let root = tempfile::tempdir().unwrap();
        let mut tx = prepared(root.path());
        tx.prepared().unwrap();
        tx.rollback("candidate did not become ready").unwrap();
        assert_eq!(
            fs::read(root.path().join("Dauerbot.app/version")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(root.path().join("data/updates/installed.json")).unwrap(),
            b"old record"
        );
        assert!(ensure_not_suppressed(&root.path().join("data"), "9.0.0", true).is_err());
        assert!(ensure_not_suppressed(&root.path().join("data"), "9.0.0", false).is_ok());
    }
    #[test]
    fn missing_rollback_material_does_not_rewrite_installation_record() {
        let root = tempfile::tempdir().unwrap();
        let tx = prepared(root.path());
        fs::remove_dir_all(&tx.journal.backup).unwrap();
        assert!(tx.rollback("lost backup").is_err());
        assert_eq!(
            fs::read(root.path().join("data/updates/installed.json")).unwrap(),
            b"new record"
        );
        assert_eq!(
            load(&root.path().join("data")).unwrap().unwrap().phase,
            "preparing"
        );
    }
    #[test]
    fn crashes_before_and_between_bundle_renames_restore_only_original_identity() {
        for between in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let tx = prepared(root.path());
            fs::remove_dir_all(&tx.journal.app).unwrap();
            if !between {
                fs::rename(&tx.journal.backup, &tx.journal.app).unwrap();
            }
            drop(tx);
            Transaction::recover(&root.path().join("data"))
                .unwrap()
                .unwrap()
                .rollback("interrupted preparation")
                .unwrap();
            assert_eq!(
                fs::read(root.path().join("Dauerbot.app/version")).unwrap(),
                b"old"
            );
            assert_eq!(
                fs::read(root.path().join("data/updates/installed.json")).unwrap(),
                b"old record"
            );
        }
    }
    #[test]
    fn legacy_rollback_cannot_bypass_committed_ownership() {
        let root = tempfile::tempdir().unwrap();
        let mut tx = prepared(root.path());
        tx.committed().unwrap();
        drop(tx);
        let error = super::super::rollback(&root.path().join("data")).unwrap_err();
        assert!(error.to_string().contains("already taken over"));
        assert_eq!(
            fs::read(root.path().join("Dauerbot.app/version")).unwrap(),
            b"new"
        );
    }
    #[test]
    fn bootstrap_claim_is_durable_and_rejects_an_unverified_executable_before_store_open() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("hfb");
        let wrong = root.path().join("wrong");
        fs::write(&executable, b"verified successor").unwrap();
        fs::write(&wrong, b"other program").unwrap();
        Bootstrap {
            id: "bootstrap-fixture".into(),
            target_version: CURRENT.into(),
            target_digest: file_hash(&executable).unwrap(),
            previous_executable: wrong.clone(),
            previous_version: "0.4.7".into(),
            owner_committed: false,
            phase: "starting".into(),
        }
        .save(root.path())
        .unwrap();
        assert!(claim_bootstrap(root.path(), &wrong).is_err());
        assert!(
            !Bootstrap::load(root.path())
                .unwrap()
                .unwrap()
                .owner_committed
        );
        claim_bootstrap(root.path(), &executable).unwrap();
        assert!(
            Bootstrap::load(root.path())
                .unwrap()
                .unwrap()
                .owner_committed
        );
        assert!(!root.path().join("bridge.sqlite3").exists());
    }
    #[test]
    fn ownership_receipt_survives_restart_and_prevents_stale_rollback() {
        let root = tempfile::tempdir().unwrap();
        let mut tx = prepared(root.path());
        tx.committed().unwrap();
        drop(tx);
        let tx = Transaction::recover(&root.path().join("data"))
            .unwrap()
            .unwrap();
        assert!(tx.rollback("lost GUI acknowledgement").is_err());
        assert_eq!(
            fs::read(root.path().join("Dauerbot.app/version")).unwrap(),
            b"new"
        );
    }
}

/// The first capable desktop may encounter a published service without handover hooks.
/// This receipt marks the irreversible startup boundary; it never contains a DB snapshot.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bootstrap {
    pub id: String,
    pub target_version: String,
    pub target_digest: String,
    pub previous_executable: PathBuf,
    pub previous_version: String,
    pub owner_committed: bool,
    pub phase: String,
}
impl Bootstrap {
    pub fn load(data: &Path) -> Result<Option<Self>> {
        match fs::read(data.join("updates/bootstrap.json")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn save(&self, data: &Path) -> Result<()> {
        config::private_dir(&data.join("updates"))?;
        config::write_private(
            &data.join("updates/bootstrap.json"),
            &serde_json::to_vec(self)?,
        )?;
        File::open(data.join("updates"))?.sync_all()?;
        Ok(())
    }
}
/// Call after exclusive daemon-lock acquisition, before opening/recovering the live
/// database or starting any ingress, dispatch, timer or native-session workers.
pub fn claim_bootstrap(data: &Path, executable: &Path) -> Result<Option<String>> {
    if let Some(mut receipt) = Bootstrap::load(data)?
        && matches!(receipt.phase.as_str(), "prepared" | "starting" | "owned")
    {
        ensure!(
            receipt.target_version == CURRENT && receipt.target_digest == file_hash(executable)?,
            "The bootstrap executable does not match its verified update receipt"
        );
        receipt.owner_committed = true;
        receipt.phase = "owned".into();
        receipt.save(data)?;
        return Ok(Some(receipt.id));
    }
    Ok(None)
}

pub fn executable_digest(path: &Path) -> Result<String> {
    file_hash(path)
}
