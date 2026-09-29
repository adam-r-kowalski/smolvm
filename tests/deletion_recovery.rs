use axum::extract::{Path, Query, State};
use smolvm::{
    api::{handlers::machines::delete_machine, state::ApiState, types::DeleteQuery},
    db::SmolvmDb,
};
use std::sync::Arc;

struct Disk(std::path::PathBuf);
impl Drop for Disk {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if self.0.exists() {
            let _ = std::process::Command::new("chflags")
                .args(["-R", "nouchg"])
                .arg(&self.0)
                .status();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn name() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "deletion-recovery-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}
fn query() -> Query<DeleteQuery> {
    Query(DeleteQuery {
        cascade: false,
        force: false,
    })
}

#[tokio::test]
async fn interrupted_before_name_marker_is_reclaimable() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let name = name();
    state.reserve_machine_name(&name, "review-token").unwrap();
    let disk = Disk(smolvm::agent::vm_data_dir(&name));
    std::fs::create_dir_all(&disk.0).unwrap(); // crash after mkdir, before writing the binding
    drop(state);
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let result = delete_machine(State(state.clone()), Path(name.clone()), query()).await;
    let pending = state.db().pending_vm_creates().unwrap().len();
    assert!(
        result.is_ok(),
        "DELETE failed for empty partial directory: {result:?}; pending={pending}"
    );
    assert_eq!(pending, 0);
    assert!(!disk.0.exists());
    assert!(matches!(
        delete_machine(State(state), Path(name), query()).await,
        Err(smolvm::api::error::ApiError::NotFound(_))
    ));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn failed_disk_removal_must_preserve_registered_ownership() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let name = name();
    let disk = Disk(smolvm::agent::ensure_vm_dir(&name).unwrap());
    let file = disk.0.join("cannot-delete");
    std::fs::write(&file, b"disk-delete failure injection").unwrap();
    assert!(std::process::Command::new("chflags")
        .arg("uchg")
        .arg(&file)
        .status()
        .unwrap()
        .success());
    let record = smolvm::config::VmRecord::new(name.clone(), 1, 512, vec![], vec![], false);
    state.db().insert_vm(&name, &record).unwrap();
    let result = delete_machine(State(state.clone()), Path(name.clone()), query()).await;
    let retained = state.db().get_vm(&name).unwrap().is_some();
    assert!(
        result.is_err() && retained,
        "DELETE result={result:?}, ownership_retained={retained}, file_still_exists={}",
        file.exists()
    );
    assert!(file.exists());
    drop(state);
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let inventory = smolvm::api::handlers::machines::list_machines(State(state.clone()))
        .await
        .unwrap();
    let wire = serde_json::to_value(inventory.0).unwrap();
    assert!(wire["machines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == name));
    assert!(std::process::Command::new("chflags")
        .arg("nouchg")
        .arg(&file)
        .status()
        .unwrap()
        .success());
    let _ = delete_machine(State(state.clone()), Path(name.clone()), query())
        .await
        .unwrap();
    assert!(!disk.0.exists());
    assert!(state.db().get_vm(&name).unwrap().is_none());
}

#[tokio::test]
async fn unbound_contents_and_mismatched_name_are_preserved_until_repaired() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let name = name();
    state.reserve_machine_name(&name, "owner").unwrap();
    let disk = Disk(smolvm::agent::vm_data_dir(&name));
    std::fs::create_dir_all(&disk.0).unwrap();
    std::fs::write(disk.0.join("unknown"), b"retain me").unwrap();
    assert!(
        delete_machine(State(state.clone()), Path(name.clone()), query())
            .await
            .is_err()
    );
    assert!(disk.0.join("unknown").exists());
    assert_eq!(state.db().pending_vm_creates().unwrap().len(), 1);
    std::fs::write(disk.0.join("name"), b"different-owner").unwrap();
    assert!(
        delete_machine(State(state.clone()), Path(name.clone()), query())
            .await
            .is_err()
    );
    assert!(disk.0.join("unknown").exists());
    assert_eq!(state.db().pending_vm_creates().unwrap().len(), 1);
    std::fs::remove_file(disk.0.join("name")).unwrap();
    std::fs::remove_file(disk.0.join("unknown")).unwrap();
    drop(state);
    let state = Arc::new(ApiState::with_db(
        SmolvmDb::open_at(&root.path().join("state.db")).unwrap(),
    ));
    let _ = delete_machine(State(state.clone()), Path(name), query())
        .await
        .unwrap();
    assert!(!disk.0.exists());
    assert!(state.db().pending_vm_creates().unwrap().is_empty());
}
