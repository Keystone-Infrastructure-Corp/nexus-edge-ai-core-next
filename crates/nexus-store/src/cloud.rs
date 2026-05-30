//! Cloud-enrollment artefact persistence (single-row table).
//!
//! Populated by `nexus-engine enroll` after a successful round-trip to
//! cloud-side `enrollment-svc /v1/enroll`. Read on every subsequent
//! `nexus-engine serve` boot to decide whether to spawn the WSS tunnel.
//!
//! See `migrations/0013_cloud_enrollment.sql` for the schema.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row as _;

use crate::Store;

/// In-memory mirror of the `cloud_enrollment` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudEnrollment {
    /// Cloud-assigned `cores.id`.
    pub core_id: String,
    /// `wss://` URL the engine dials on boot.
    pub gateway_url: String,
    /// PEM-encoded mTLS leaf certificate signed by the internal CA.
    pub cert_pem: String,
    /// PEM-encoded PKCS#8 private key pairing with `cert_pem`.
    pub private_key_pem: String,
    /// PEM-encoded internal CA chain the engine validates the gateway
    /// against.
    pub ca_chain_pem: String,
    /// Initial entitlement JWT (`aud = "nexus-edge"`).
    pub entitlement_jwt: String,
    /// Optional Ed25519 verifying-key PEM (forward-compat from the
    /// cloud `EnrollResponse`).
    pub signing_key_pem: Option<String>,
    /// Optional `kid` to bind the verifying key to.
    pub signing_kid: Option<String>,
    /// UTC timestamp of the enrollment round-trip.
    pub enrolled_at: DateTime<Utc>,
    /// Phase 2 · Step 2.9 — when `nexus-engine enroll --keep-history`
    /// is used, this is the cutoff timestamp (computed as
    /// `entitlement.iat - history_days`) and signals to the next
    /// `serve` boot that the local motion-clip backlog since this
    /// instant should be replayed through the cloud outbox as
    /// `clip_replicated` envelopes with `attached_history: true`.
    /// NULL after a successful replay (or when `--keep-history` was
    /// not passed). The replay task NULLs the column only after the
    /// whole window has been re-sent; partial replays roll back to
    /// the same cutoff, idempotent via the cloud-side
    /// `ON CONFLICT (core_id, edge_clip_id) DO UPDATE`.
    pub attach_replay_after: Option<DateTime<Utc>>,
    /// v0.1.36 (M-HTTPS Phase 3) — PEM-encoded `serverAuth`-EKU
    /// leaf the cloud minted for the engine's local HTTPS listener.
    /// `None` for pre-v0.1.36 enrollments and for any enrollment
    /// where the cloud-side mint failed (the engine falls back to
    /// its self-signed leaf in both cases). The paired private key
    /// lives in [`Self::server_private_key_pem`].
    pub server_cert_pem: Option<String>,
    /// v0.1.36 (M-HTTPS Phase 3) — PEM-encoded PKCS#8 private key
    /// pairing with [`Self::server_cert_pem`]. Generated
    /// edge-side during enrollment so the key never leaves the
    /// appliance. Always paired: both `Some` or both `None`.
    pub server_private_key_pem: Option<String>,
}

impl Store {
    /// Persist (or overwrite) the cloud enrollment row.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StoreError::Sqlx`] on database failure.
    pub async fn set_cloud_enrollment(&self, e: &CloudEnrollment) -> Result<(), crate::StoreError> {
        sqlx::query(
            r#"
            INSERT INTO cloud_enrollment
                (id, core_id, gateway_url, cert_pem, private_key_pem,
                 ca_chain_pem, entitlement_jwt, signing_key_pem, signing_kid,
                 attach_replay_after, server_cert_pem, server_private_key_pem)
            VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET
                core_id                = excluded.core_id,
                gateway_url            = excluded.gateway_url,
                cert_pem               = excluded.cert_pem,
                private_key_pem        = excluded.private_key_pem,
                ca_chain_pem           = excluded.ca_chain_pem,
                entitlement_jwt        = excluded.entitlement_jwt,
                signing_key_pem        = excluded.signing_key_pem,
                signing_kid            = excluded.signing_kid,
                attach_replay_after    = excluded.attach_replay_after,
                server_cert_pem        = excluded.server_cert_pem,
                server_private_key_pem = excluded.server_private_key_pem,
                enrolled_at            = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
            "#,
        )
        .bind(&e.core_id)
        .bind(&e.gateway_url)
        .bind(&e.cert_pem)
        .bind(&e.private_key_pem)
        .bind(&e.ca_chain_pem)
        .bind(&e.entitlement_jwt)
        .bind(e.signing_key_pem.as_deref())
        .bind(e.signing_kid.as_deref())
        .bind(e.attach_replay_after.map(|ts| ts.to_rfc3339()))
        .bind(e.server_cert_pem.as_deref())
        .bind(e.server_private_key_pem.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the single cloud-enrollment row, if present.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StoreError::Sqlx`] on database failure.
    pub async fn get_cloud_enrollment(&self) -> Result<Option<CloudEnrollment>, crate::StoreError> {
        let row = sqlx::query(
            r#"
            SELECT core_id, gateway_url, cert_pem, private_key_pem,
                   ca_chain_pem, entitlement_jwt, signing_key_pem,
                   signing_kid, enrolled_at, attach_replay_after,
                   server_cert_pem, server_private_key_pem
              FROM cloud_enrollment
             WHERE id = 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        let enrolled_at_str: String = row.try_get("enrolled_at")?;
        let enrolled_at = DateTime::parse_from_rfc3339(&enrolled_at_str)
            .map_err(|e| crate::StoreError::Decode(format!("enrolled_at parse: {e}")))?
            .with_timezone(&Utc);
        let attach_replay_after = row
            .try_get::<Option<String>, _>("attach_replay_after")?
            .map(|s| {
                DateTime::parse_from_rfc3339(&s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(|e| {
                        crate::StoreError::Decode(format!("attach_replay_after parse: {e}"))
                    })
            })
            .transpose()?;
        Ok(Some(CloudEnrollment {
            core_id: row.try_get("core_id")?,
            gateway_url: row.try_get("gateway_url")?,
            cert_pem: row.try_get("cert_pem")?,
            private_key_pem: row.try_get("private_key_pem")?,
            ca_chain_pem: row.try_get("ca_chain_pem")?,
            entitlement_jwt: row.try_get("entitlement_jwt")?,
            signing_key_pem: row.try_get("signing_key_pem")?,
            signing_kid: row.try_get("signing_kid")?,
            enrolled_at,
            attach_replay_after,
            server_cert_pem: row.try_get("server_cert_pem")?,
            server_private_key_pem: row.try_get("server_private_key_pem")?,
        }))
    }

    /// Phase 2 · Step 2.9 — clear the attach-history replay cursor
    /// after a successful drain of the historical motion-clip backlog
    /// through the cloud outbox. Idempotent: clearing an already-NULL
    /// column is a no-op single-row UPDATE.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StoreError::Sqlx`] on database failure.
    pub async fn clear_attach_replay_after(&self) -> Result<(), crate::StoreError> {
        sqlx::query(
            r#"
            UPDATE cloud_enrollment
               SET attach_replay_after = NULL
             WHERE id = 1
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete the single `cloud_enrollment` row. Idempotent: no-op
    /// when the row is already absent. The next `serve` boot will
    /// see no enrollment and skip the WSS tunnel supervisor.
    ///
    /// Used by the cloud-initiated detach flow (`DELETE
    /// /v1/admin/cloud/enrollment`) and the operator-driven local
    /// "Disconnect from cloud" affordance. An already-running tunnel
    /// is NOT torn down by this call — the in-process TunnelClient
    /// holds the cert/key material it was built from and persists
    /// until the engine restarts. The handler advertises that
    /// restart-required behaviour in its tracing output.
    ///
    /// # Errors
    ///
    /// Returns [`crate::StoreError::Sqlx`] on database failure.
    pub async fn clear_cloud_enrollment(&self) -> Result<(), crate::StoreError> {
        sqlx::query(
            r#"
            DELETE FROM cloud_enrollment WHERE id = 1
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use nexus_config::StoreConfig;
    use tempfile::TempDir;

    async fn fresh_store() -> (Store, TempDir) {
        let tmp = TempDir::new().unwrap();
        let cfg = StoreConfig {
            url: format!("sqlite://{}/store.db?mode=rwc", tmp.path().display()),
            ..StoreConfig::default()
        };
        let store = Store::open(&cfg).await.expect("open store");
        (store, tmp)
    }

    fn sample() -> CloudEnrollment {
        CloudEnrollment {
            core_id: "11111111-2222-3333-4444-555555555555".into(),
            gateway_url: "wss://gateway.test/v1/tunnel".into(),
            cert_pem: "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".into(),
            private_key_pem: "-----BEGIN PRIVATE KEY-----\nBBBB\n-----END PRIVATE KEY-----\n"
                .into(),
            ca_chain_pem: "-----BEGIN CERTIFICATE-----\nCCCC\n-----END CERTIFICATE-----\n".into(),
            entitlement_jwt: "eyJ.fake.jwt".into(),
            signing_key_pem: Some(
                "-----BEGIN PUBLIC KEY-----\nDDDD\n-----END PUBLIC KEY-----\n".into(),
            ),
            signing_kid: Some("kid-1".into()),
            enrolled_at: Utc::now(), // overwritten by the DB default
            attach_replay_after: None,
            server_cert_pem: None,
            server_private_key_pem: None,
        }
    }

    #[tokio::test]
    async fn round_trips_through_storage() {
        let (store, _tmp) = fresh_store().await;
        assert!(store.get_cloud_enrollment().await.unwrap().is_none());
        store.set_cloud_enrollment(&sample()).await.unwrap();
        let got = store
            .get_cloud_enrollment()
            .await
            .unwrap()
            .expect("present");
        assert_eq!(got.core_id, "11111111-2222-3333-4444-555555555555");
        assert_eq!(got.gateway_url, "wss://gateway.test/v1/tunnel");
        assert_eq!(got.signing_kid.as_deref(), Some("kid-1"));
    }

    #[tokio::test]
    async fn upsert_overwrites_in_place() {
        let (store, _tmp) = fresh_store().await;
        store.set_cloud_enrollment(&sample()).await.unwrap();
        let mut second = sample();
        second.core_id = "99999999-9999-9999-9999-999999999999".into();
        second.signing_kid = Some("kid-2".into());
        store.set_cloud_enrollment(&second).await.unwrap();
        let got = store
            .get_cloud_enrollment()
            .await
            .unwrap()
            .expect("present");
        assert_eq!(got.core_id, "99999999-9999-9999-9999-999999999999");
        assert_eq!(got.signing_kid.as_deref(), Some("kid-2"));
    }

    #[tokio::test]
    async fn attach_replay_after_round_trips_and_clears() {
        // Phase 2 · Step 2.9 — `--keep-history` writes a cutoff; the
        // boot-time replay task NULLs it on success.
        let (store, _tmp) = fresh_store().await;
        let mut e = sample();
        let cutoff = Utc::now() - chrono::Duration::days(30);
        e.attach_replay_after = Some(cutoff);
        store.set_cloud_enrollment(&e).await.unwrap();
        let got = store.get_cloud_enrollment().await.unwrap().expect("row");
        let got_cutoff = got.attach_replay_after.expect("cutoff persisted");
        // Sub-millisecond drift through RFC3339 string round-trip.
        assert!((got_cutoff - cutoff).num_milliseconds().abs() < 2);

        store.clear_attach_replay_after().await.unwrap();
        let cleared = store.get_cloud_enrollment().await.unwrap().expect("row");
        assert!(cleared.attach_replay_after.is_none());

        // Idempotent clear.
        store.clear_attach_replay_after().await.unwrap();
        let still_clear = store.get_cloud_enrollment().await.unwrap().expect("row");
        assert!(still_clear.attach_replay_after.is_none());
    }

    #[tokio::test]
    async fn clear_cloud_enrollment_removes_row_and_is_idempotent() {
        let (store, _tmp) = fresh_store().await;
        // Empty → clearing is a no-op, not an error.
        store.clear_cloud_enrollment().await.unwrap();
        assert!(store.get_cloud_enrollment().await.unwrap().is_none());

        // Populated → clearing returns the table to empty.
        store.set_cloud_enrollment(&sample()).await.unwrap();
        assert!(store.get_cloud_enrollment().await.unwrap().is_some());
        store.clear_cloud_enrollment().await.unwrap();
        assert!(store.get_cloud_enrollment().await.unwrap().is_none());

        // Second clear on already-empty is still a no-op.
        store.clear_cloud_enrollment().await.unwrap();
        assert!(store.get_cloud_enrollment().await.unwrap().is_none());
    }
}
