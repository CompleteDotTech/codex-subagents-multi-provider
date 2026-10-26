//! Host-written effective-authority epochs; rollout import/rebuild never touches these rows.
use super::StateRuntime;
use anyhow::Context;
use codex_protocol::ThreadId;
use sqlx::Row;
use uuid::Uuid;

pub const NATIVE_AUTHORITY_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAuthorityEpoch {
    pub version: u32,
    pub thread_id: ThreadId,
    pub root_thread_id: ThreadId,
    pub parent_thread_id: Option<ThreadId>,
    pub generation: Uuid,
}

impl NativeAuthorityEpoch {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.version == NATIVE_AUTHORITY_VERSION,
            "unsupported native authority version"
        );
        anyhow::ensure!(
            self.parent_thread_id != Some(self.thread_id),
            "self-owned native authority"
        );
        anyhow::ensure!(
            self.parent_thread_id.is_some() || self.thread_id == self.root_thread_id,
            "native root authority identity mismatch"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeAuthorityReceipt {
    pub epoch: NativeAuthorityEpoch,
    /// SHA256 of an explicit, nonsecret effective-grant projection only.
    pub fingerprint: String,
}

impl NativeAuthorityReceipt {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.epoch.validate()?;
        anyhow::ensure!(
            self.fingerprint.len() == 64
                && self
                    .fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid native authority fingerprint"
        );
        Ok(())
    }
}

fn epoch_from_row(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<NativeAuthorityEpoch> {
    let epoch = NativeAuthorityEpoch {
        version: u32::try_from(row.try_get::<i64, _>("version")?)?,
        thread_id: row.try_get::<String, _>("thread_id")?.parse()?,
        root_thread_id: row.try_get::<String, _>("root_thread_id")?.parse()?,
        parent_thread_id: row
            .try_get::<Option<String>, _>("parent_thread_id")?
            .map(|id| id.parse())
            .transpose()?,
        generation: row.try_get::<String, _>("generation")?.parse()?,
    };
    epoch.validate()?;
    Ok(epoch)
}

impl StateRuntime {
    /// Invalidate any old sealed receipt before publishing a new host Session.
    /// Same target IDs can never be reassigned to a different host tree/owner.
    pub async fn begin_native_authority_epoch(
        &self,
        epoch: &NativeAuthorityEpoch,
    ) -> anyhow::Result<()> {
        epoch.validate()?;
        let result = sqlx::query(
            "INSERT INTO native_authority_receipts
            (thread_id,root_thread_id,parent_thread_id,version,generation,fingerprint,sealed)
            VALUES (?,?,?,?,?,NULL,0)
            ON CONFLICT(thread_id) DO UPDATE SET version=excluded.version,
                generation=excluded.generation,fingerprint=NULL,sealed=0
            WHERE native_authority_receipts.root_thread_id=excluded.root_thread_id
              AND native_authority_receipts.parent_thread_id IS excluded.parent_thread_id",
        )
        .bind(epoch.thread_id.to_string())
        .bind(epoch.root_thread_id.to_string())
        .bind(epoch.parent_thread_id.map(|id| id.to_string()))
        .bind(i64::from(epoch.version))
        .bind(epoch.generation.to_string())
        .execute(self.pool.as_ref())
        .await
        .context("begin native authority epoch")?;
        anyhow::ensure!(
            result.rows_affected() == 1,
            "native authority host ownership changed"
        );
        Ok(())
    }

    /// Fence old unload writers before any reopened runtime can execute startup.
    pub async fn invalidate_native_authority_receipt(
        &self,
        thread: ThreadId,
        fence: Uuid,
    ) -> anyhow::Result<()> {
        sqlx::query("UPDATE native_authority_receipts SET generation=?,fingerprint=NULL,sealed=0 WHERE thread_id=?")
            .bind(fence.to_string()).bind(thread.to_string()).execute(self.pool.as_ref()).await
            .context("invalidate native authority receipt")?;
        Ok(())
    }

    /// A late unload cannot seal an older generation over a newly opened Session.
    pub async fn seal_native_authority_receipt(
        &self,
        receipt: &NativeAuthorityReceipt,
    ) -> anyhow::Result<bool> {
        receipt.validate()?;
        let result = sqlx::query("UPDATE native_authority_receipts SET fingerprint=?,sealed=1
            WHERE thread_id=? AND root_thread_id=? AND parent_thread_id IS ? AND version=? AND generation=?")
            .bind(&receipt.fingerprint).bind(receipt.epoch.thread_id.to_string())
            .bind(receipt.epoch.root_thread_id.to_string())
            .bind(receipt.epoch.parent_thread_id.map(|id| id.to_string()))
            .bind(i64::from(receipt.epoch.version)).bind(receipt.epoch.generation.to_string())
            .execute(self.pool.as_ref()).await.context("seal native authority receipt")?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn read_native_authority_epoch(
        &self,
        thread: ThreadId,
    ) -> anyhow::Result<Option<NativeAuthorityEpoch>> {
        sqlx::query("SELECT thread_id,root_thread_id,parent_thread_id,version,generation FROM native_authority_receipts WHERE thread_id=?")
            .bind(thread.to_string()).fetch_optional(self.pool.as_ref()).await?
            .map(|row| epoch_from_row(&row)).transpose()
    }

    pub async fn read_native_authority_receipt(
        &self,
        thread: ThreadId,
    ) -> anyhow::Result<Option<NativeAuthorityReceipt>> {
        let Some(row) = sqlx::query("SELECT thread_id,root_thread_id,parent_thread_id,version,generation,fingerprint FROM native_authority_receipts WHERE thread_id=? AND sealed=1")
            .bind(thread.to_string()).fetch_optional(self.pool.as_ref()).await? else { return Ok(None); };
        let receipt = NativeAuthorityReceipt {
            epoch: epoch_from_row(&row)?,
            fingerprint: row.try_get("fingerprint")?,
        };
        receipt.validate()?;
        Ok(Some(receipt))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteConfig;
    use crate::runtime::test_support::test_thread_metadata;
    use codex_utils_absolute_path::test_support::PathExt;

    async fn runtime() -> (tempfile::TempDir, Arc<StateRuntime>) {
        let home = tempfile::tempdir().unwrap();
        let runtime = StateRuntime::init(
            SqliteConfig::new_for_testing(home.path().abs()),
            "test".into(),
        )
        .await
        .unwrap();
        (home, runtime)
    }
    use std::sync::Arc;
    fn root_epoch() -> NativeAuthorityEpoch {
        let root = ThreadId::new();
        NativeAuthorityEpoch {
            version: NATIVE_AUTHORITY_VERSION,
            thread_id: root,
            root_thread_id: root,
            parent_thread_id: None,
            generation: Uuid::now_v7(),
        }
    }

    #[tokio::test]
    async fn native_authority_active_invalidation_and_generation_cas_never_restore_an_old_seal() {
        let (_home, runtime) = runtime().await;
        let old = root_epoch();
        runtime.begin_native_authority_epoch(&old).await.unwrap();
        assert!(
            runtime
                .read_native_authority_receipt(old.thread_id)
                .await
                .unwrap()
                .is_none()
        );
        let old_receipt = NativeAuthorityReceipt {
            epoch: old.clone(),
            fingerprint: "a".repeat(64),
        };
        assert!(
            runtime
                .seal_native_authority_receipt(&old_receipt)
                .await
                .unwrap()
        );
        assert_eq!(
            runtime
                .read_native_authority_receipt(old.thread_id)
                .await
                .unwrap(),
            Some(old_receipt.clone())
        );
        let fence = Uuid::now_v7();
        runtime
            .invalidate_native_authority_receipt(old.thread_id, fence)
            .await
            .unwrap();
        assert!(
            runtime
                .read_native_authority_receipt(old.thread_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !runtime
                .seal_native_authority_receipt(&old_receipt)
                .await
                .unwrap(),
            "late old unload must not reseal after startup fencing"
        );
        let mut new = old.clone();
        new.generation = Uuid::now_v7();
        runtime.begin_native_authority_epoch(&new).await.unwrap();
        let new_receipt = NativeAuthorityReceipt {
            epoch: new.clone(),
            fingerprint: "b".repeat(64),
        };
        assert!(
            runtime
                .seal_native_authority_receipt(&new_receipt)
                .await
                .unwrap()
        );
        assert!(
            !runtime
                .seal_native_authority_receipt(&old_receipt)
                .await
                .unwrap()
        );
        assert_eq!(
            runtime
                .read_native_authority_receipt(old.thread_id)
                .await
                .unwrap(),
            Some(new_receipt)
        );
        let mut foreign = new;
        foreign.root_thread_id = ThreadId::new();
        foreign.parent_thread_id = Some(foreign.root_thread_id);
        assert!(
            runtime
                .begin_native_authority_epoch(&foreign)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_authority_is_not_minted_or_erased_by_rebuildable_metadata_but_deletion_cleans_it()
     {
        let (home, runtime) = runtime().await;
        let epoch = root_epoch();
        let metadata =
            test_thread_metadata(home.path(), epoch.thread_id, home.path().to_path_buf());
        runtime.upsert_thread(&metadata).await.unwrap();
        assert!(
            runtime
                .read_native_authority_epoch(epoch.thread_id)
                .await
                .unwrap()
                .is_none(),
            "rollout-derived metadata never mints authority"
        );
        runtime.begin_native_authority_epoch(&epoch).await.unwrap();
        let receipt = NativeAuthorityReceipt {
            epoch: epoch.clone(),
            fingerprint: "c".repeat(64),
        };
        runtime
            .seal_native_authority_receipt(&receipt)
            .await
            .unwrap();
        runtime.upsert_thread(&metadata).await.unwrap();
        assert_eq!(
            runtime
                .read_native_authority_receipt(epoch.thread_id)
                .await
                .unwrap(),
            Some(receipt)
        );
        runtime.delete_thread(epoch.thread_id).await.unwrap();
        assert!(
            runtime
                .read_native_authority_epoch(epoch.thread_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn native_authority_malformed_version_digest_and_closed_store_fail_closed() {
        let (_home, runtime) = runtime().await;
        let epoch = root_epoch();
        runtime.begin_native_authority_epoch(&epoch).await.unwrap();
        assert!(
            runtime
                .seal_native_authority_receipt(&NativeAuthorityReceipt {
                    epoch: epoch.clone(),
                    fingerprint: "secret-is-not-a-digest".into()
                })
                .await
                .is_err()
        );
        sqlx::query("UPDATE native_authority_receipts SET version=99 WHERE thread_id=?")
            .bind(epoch.thread_id.to_string())
            .execute(runtime.pool.as_ref())
            .await
            .unwrap();
        assert!(
            runtime
                .read_native_authority_epoch(epoch.thread_id)
                .await
                .is_err()
        );
        runtime.close().await;
        assert!(
            runtime
                .invalidate_native_authority_receipt(epoch.thread_id, Uuid::now_v7())
                .await
                .is_err()
        );
    }
}
