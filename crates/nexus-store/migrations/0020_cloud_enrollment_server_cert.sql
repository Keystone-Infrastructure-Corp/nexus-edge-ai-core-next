-- v0.1.36 (M-HTTPS Phase 3): persistence for the cloud-issued
-- TLS *server* leaf used by the engine's local admin/UI HTTPS
-- listener.
--
-- The cloud_enrollment table already stores the mTLS *client*
-- leaf (the cert the engine presents on the WSS tunnel to the
-- edge-gateway). v0.1.36 adds a second, distinct leaf with the
-- same Subject URN tuple but `serverAuth` EKU, so a browser
-- visiting `https://nexus.local/` validates against the cloud's
-- internal CA instead of the bootstrap self-signed leaf.
--
-- Both columns are NULLable because:
--   * Pre-v0.1.36 enrolled cores have no cloud-issued server
--     leaf — the engine falls back to its self-signed leaf
--     (still secure on the LAN, just triggers the browser
--     warning) until the operator re-enrolls.
--   * The mint is best-effort cloud-side: if the cloud rejects
--     the server CSR or the rcgen step fails, enrollment still
--     succeeds with NULLs here, and the engine logs + falls
--     back to self-signed.

ALTER TABLE cloud_enrollment ADD COLUMN server_cert_pem TEXT;
ALTER TABLE cloud_enrollment ADD COLUMN server_private_key_pem TEXT;
