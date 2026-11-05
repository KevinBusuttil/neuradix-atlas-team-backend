//! C4: server-history identity for the replication log.
//!
//! Every mutation carries a link of a hash chain over the company log:
//!
//! ```text
//! chain(v) = SHA-256( chain(v-1) || v as 8 big-endian bytes || mutation id
//!                     || 0x00 || SHA-256(canonical record JSON) )
//! chain(0) = 32 zero bytes
//! ```
//!
//! A device remembers the chain value at the newest version it has seen
//! (its checkpoint). Before it syncs again it asks for the chain value at
//! that version. A server restored from a backup keeps the chain up to the
//! backup, but anything written after the restore builds on the older
//! value, so the hash at the device's checkpoint differs, or that version
//! does not exist yet. That holds even when the restored log has since
//! grown past the device's cursor, and even though the backup restored the
//! same tables: no generation number stored in the database is trusted.
//!
//! Rows written before this migration are linked lazily, in version order,
//! the first time the company's log is appended to or checked.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The protocol revision reported by `GET …/sync/history`. Clients that see
/// no such route (an older server) fall back to the head-behind-cursor
/// check, which catches only a restore with no later activity.
pub const HISTORY_PROTOCOL: i64 = 1;

pub const GENESIS: [u8; 32] = [0; 32];

/// The record as hashed: its JSON with keys sorted (serde_json's default
/// map) and without `syncVersion`, which the log assigns.
pub fn canonical(record: &Value) -> Vec<u8> {
    let mut v = record.clone();
    if let Value::Object(map) = &mut v {
        map.remove("syncVersion");
    }
    serde_json::to_vec(&v).unwrap_or_default()
}

/// The next chain value.
pub fn link(prev: &[u8], version: i64, mutation_id: &str, record: &Value) -> Vec<u8> {
    let body = Sha256::digest(canonical(record));
    let mut h = Sha256::new();
    h.update(prev);
    h.update(version.to_be_bytes());
    h.update(mutation_id.as_bytes());
    h.update([0u8]);
    h.update(body);
    h.finalize().to_vec()
}

/// Where the company log stands, and its chain value at a version the
/// caller asked about.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryCheckpoint {
    pub protocol: i64,
    pub head: i64,
    pub head_hash: String,
    pub at: i64,
    /// None when `at` is past the head: the log is shorter than the caller
    /// remembers, so it was restored.
    pub at_hash: Option<String>,
}

/// One official record a recovering device had and the restored server no
/// longer has.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LostRecord {
    pub doctype: String,
    pub document_id: String,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub posting_date: Option<String>,
    #[serde(default)]
    pub total: Option<f64>,
}

/// `POST …/commands/report-lost-history`: what one device found missing
/// after a restore. Idempotent on `recovery_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LostHistoryReport {
    pub recovery_id: String,
    #[serde(default)]
    pub lost: Vec<LostRecord>,
}

/// The numbering floor a lost Team number implies: `SINV-00042` for a
/// Sales Invoice keeps the series from issuing 42 again. Numbers in any
/// other format (a Solo number, a till series) are not Team series values
/// and set no floor.
pub fn floor_for(doctype: &str, number: &str) -> Option<(&'static str, i64)> {
    let key = crate::posting::model::series_key(doctype)?;
    let digits = number.strip_prefix(key)?.strip_prefix('-')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| (key, n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_chain_depends_on_order_ids_and_content() {
        let a = json!({"id": "m1", "payload": {"b": 1, "a": 2}});
        let a_reordered = json!({"payload": {"a": 2, "b": 1}, "id": "m1", "syncVersion": "9"});
        let one = link(&GENESIS, 1, "m1", &a);
        assert_eq!(one, link(&GENESIS, 1, "m1", &a_reordered));
        assert_ne!(one, link(&GENESIS, 2, "m1", &a));
        assert_ne!(one, link(&GENESIS, 1, "m2", &a));
        assert_ne!(
            one,
            link(&GENESIS, 1, "m1", &json!({"id": "m1", "payload": {"a": 3}}))
        );
        assert_ne!(link(&one, 2, "m2", &a), link(&GENESIS, 2, "m2", &a));
    }

    #[test]
    fn floors_only_from_team_numbers() {
        assert_eq!(floor_for("Sales Invoice", "SINV-00042"), Some(("SINV", 42)));
        assert_eq!(floor_for("Sales Invoice", "SINV-.2026.-0042"), None);
        assert_eq!(floor_for("Sales Invoice", "PINV-00042"), None);
        assert_eq!(floor_for("Journal Entry", "JV-00001"), Some(("JV", 1)));
        assert_eq!(floor_for("Quotation", "QTN-00001"), None);
        assert_eq!(floor_for("Sales Invoice", "SINV-"), None);
    }
}
