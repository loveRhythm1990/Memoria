use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

type Registry = Mutex<HashMap<String, Weak<KeyedGate>>>;

/// Gates are shared by callers and live only while callers hold them.
/// Do not evict live gates: that would let a second initializer bypass waiters.
#[derive(Clone, Default)]
pub(crate) struct KeyedGates {
    registry: Arc<Registry>,
}

pub(crate) struct KeyedGate {
    pub(crate) mutex: tokio::sync::Mutex<()>,
    key: String,
    registry: Weak<Registry>,
}

impl KeyedGates {
    pub(crate) fn get(&self, key: &str) -> Arc<KeyedGate> {
        let mut registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = registry.get(key).and_then(Weak::upgrade) {
            return entry;
        }
        let entry = Arc::new(KeyedGate {
            mutex: tokio::sync::Mutex::new(()),
            key: key.to_owned(),
            registry: Arc::downgrade(&self.registry),
        });
        registry.insert(key.to_owned(), Arc::downgrade(&entry));
        entry
    }
}

impl Drop for KeyedGate {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
        // A caller can replace an expired Weak while this destructor waits for
        // the registry lock. Never remove that caller's replacement gate.
        if registry
            .get(&self.key)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            registry.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyedGate, KeyedGates};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::oneshot;

    #[test]
    fn clones_share_live_gates_and_reclaim_idle_entries() {
        let locks = KeyedGates::default();
        let first = locks.get("same-user");
        let second = locks.clone().get("same-user");
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        assert_eq!(locks.registry.lock().unwrap().len(), 1);
        drop(second);
        assert!(locks.registry.lock().unwrap().is_empty());

        for index in 0..100 {
            drop(locks.get(&format!("one-time-user-{index}")));
        }
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn different_users_do_not_share_an_initialization_gate() {
        let locks = KeyedGates::default();
        let first = locks.get("first-user");
        let _guard = first.mutex.lock().await;
        let second = locks.get("second-user");
        assert!(second.mutex.try_lock().is_ok());
        assert!(locks.get("first-user").mutex.try_lock().is_err());
    }

    #[tokio::test]
    async fn cancelling_a_waiter_preserves_the_active_gate() {
        let locks = KeyedGates::default();
        let first = locks.get("same-user");
        let guard = first.mutex.lock().await;
        let shared = locks.clone();
        let (started, waiting) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let entry = shared.get("same-user");
            started.send(()).unwrap();
            let _guard = entry.mutex.lock().await;
        });
        waiting.await.unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        assert!(Arc::ptr_eq(&first, &locks.get("same-user")));
        drop(guard);
        drop(first);
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cancelling_an_initializer_releases_its_waiters() {
        let locks = KeyedGates::default();
        let shared = locks.clone();
        let (entered, initialized) = oneshot::channel();
        let initializer = tokio::spawn(async move {
            let entry = shared.get("same-user");
            let _guard = entry.mutex.lock().await;
            entered.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        initialized.await.unwrap();
        let shared = locks.clone();
        let (started, waiting) = oneshot::channel();
        let waiter = tokio::spawn(async move {
            let entry = shared.get("same-user");
            started.send(()).unwrap();
            let _guard = entry.mutex.lock().await;
        });
        waiting.await.unwrap();
        initializer.abort();
        assert!(initializer.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("waiter must not be stranded")
            .unwrap();
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[test]
    fn expired_gate_cleanup_does_not_remove_a_replacement() {
        let locks = KeyedGates::default();
        let old = locks.get("same-user");
        // Model replacement after the final strong reference expires but before
        // its destructor obtains the registry mutex.
        let replacement = Arc::new(KeyedGate {
            mutex: tokio::sync::Mutex::new(()),
            key: "same-user".into(),
            registry: Arc::downgrade(&locks.registry),
        });
        locks
            .registry
            .lock()
            .unwrap()
            .insert("same-user".into(), Arc::downgrade(&replacement));
        drop(old);
        assert!(Arc::ptr_eq(&replacement, &locks.get("same-user")));
        drop(replacement);
        assert!(locks.registry.lock().unwrap().is_empty());
    }

    #[test]
    fn gate_can_outlive_the_registry() {
        let locks = KeyedGates::default();
        let entry = locks.get("same-user");
        drop(locks);
        drop(entry);
    }
}
