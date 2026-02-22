use crate::domain::registry::DomainRegistry;
use crate::index::store::IndexStore;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// Watch the vault directory for file changes and update the index.
/// If a registry is provided, changes under `vault/domains/` trigger a registry rebuild.
pub async fn watch_vault(
    vault_root: PathBuf,
    index: Arc<IndexStore>,
    registry: Option<Arc<RwLock<DomainRegistry>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (tx, mut rx) = mpsc::channel::<PathBuf>(100);

    let vault_root_clone = vault_root.clone();
    std::thread::spawn(move || {
        let rt_tx = tx;
        let mut watcher = match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
            if let Ok(event) = res {
                match event.kind {
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                        for path in event.paths {
                            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                                let _ = rt_tx.blocking_send(path);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("wardwell: vault watcher failed to start: {e}");
                return;
            }
        };

        if let Err(e) = watcher.watch(&vault_root_clone, RecursiveMode::Recursive) {
            eprintln!("wardwell: could not watch {}: {e}", vault_root_clone.display());
            return;
        }

        // Block this thread forever to keep the watcher alive
        std::thread::park();
    });

    let domains_prefix = vault_root.join("domains");

    // Process file change events
    let vault_root = vault_root.clone();
    while let Some(path) = rx.recv().await {
        // Check if this is a domain file change → rebuild registry
        if path.starts_with(&domains_prefix)
            && let Some(ref reg) = registry
        {
            let new_registry = DomainRegistry::from_vault(&vault_root);
            let mut write_guard = reg.write().await;
            *write_guard = new_registry;
            eprintln!("wardwell: domain registry rebuilt");
        }

        if path.exists() {
            // File created or modified — upsert
            match crate::vault::reader::read_file(&path) {
                Ok(vf) => {
                    match index.upsert(&vf, &vault_root) {
                        Ok(true) => eprintln!("wardwell: indexed {}", path.display()),
                        Ok(false) => {} // unchanged
                        Err(e) => eprintln!("wardwell: index error for {}: {e}", path.display()),
                    }
                }
                Err(e) => eprintln!("wardwell: parse error for {}: {e}", path.display()),
            }
        } else {
            // File removed
            let relative = path
                .strip_prefix(&vault_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if let Err(e) = index.remove(&relative) {
                eprintln!("wardwell: remove error for {relative}: {e}");
            }
        }
    }

    Ok(())
}
