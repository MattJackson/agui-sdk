//! `InterruptStore` implemented outside the crate, as a database-backed store
//! would be: only public constructors and helpers are available here.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Mutex;

use agui_sdk::ids::{InterruptId, ThreadId};
use agui_sdk::interrupt::{Interrupt, ResumeEntry, ResumeStatus};
use agui_sdk::server::helpers::{InterruptStore, ResumeCheck};
use agui_sdk::Result;

/// Stands in for a shared database table keyed by thread.
#[derive(Default)]
struct TableStore {
    rows: Mutex<HashMap<ThreadId, Vec<Interrupt>>>,
}

impl InterruptStore for TableStore {
    async fn record(&self, thread: &ThreadId, interrupts: Vec<Interrupt>) -> Result<()> {
        self.rows.lock().unwrap().insert(thread.clone(), interrupts);
        Ok(())
    }

    async fn check(&self, thread: &ThreadId, entries: &[ResumeEntry]) -> Result<ResumeCheck> {
        let rows = self.rows.lock().unwrap();
        let open = rows.get(thread).map_or(&[][..], Vec::as_slice);
        Ok(ResumeCheck::classify(open, entries))
    }

    async fn resolve(&self, thread: &ThreadId, interrupt: &InterruptId) -> Result<()> {
        if let Some(open) = self.rows.lock().unwrap().get_mut(thread) {
            open.retain(|i| &i.id != interrupt);
        }
        Ok(())
    }
}

/// covers: INTR-18, INTR-20
#[test]
fn an_external_store_sorts_resume_entries() {
    let store = TableStore::default();
    let thread = ThreadId::from("t-1");
    futures_executor::block_on(async {
        store
            .record(
                &thread,
                vec![
                    Interrupt::new("i-1", "approval"),
                    Interrupt::new("i-2", "approval"),
                ],
            )
            .await
            .unwrap();
        let entries = [
            ResumeEntry::new("i-1", ResumeStatus::Resolved),
            ResumeEntry::new("i-9", ResumeStatus::Resolved),
        ];
        let check = store.check(&thread, &entries).await.unwrap();
        assert_eq!(check.recognised, vec![entries[0].clone()]);
        assert_eq!(check.unrecognised, vec![entries[1].clone()]);
        assert_eq!(check.uncovered, vec![Interrupt::new("i-2", "approval")]);

        store.resolve(&thread, &"i-2".into()).await.unwrap();
        let check = store.check(&thread, &entries[..1]).await.unwrap();
        assert_eq!(
            check,
            ResumeCheck::new(vec![entries[0].clone()], Vec::new(), Vec::new())
        );
    });
}

#[test]
fn an_empty_check_is_the_default() {
    assert_eq!(
        ResumeCheck::default(),
        ResumeCheck::new(Vec::new(), Vec::new(), Vec::new())
    );
}
