//! Talk-down library, plays and arming storage (migration 0040).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;

use chrono::{Duration, Utc};
use nexus_config::{SpeakerConfig, StoreConfig};
use nexus_store::{AudioKind, AudioStatus, NewAudio, NewPlay, PlayFilter, Store, StoreError};
use tempfile::TempDir;

async fn fresh_store() -> (Store, TempDir) {
    let dir = tempfile::tempdir().expect("tmpdir");
    let cfg = StoreConfig {
        url: format!("sqlite:{}?mode=rwc", dir.path().join("nexus.db").display()),
        seed_from_config: false,
        duckdb_attach: false,
        duckdb_path: PathBuf::from("/tmp/unused.duckdb"),
    };
    (Store::open(&cfg).await.expect("Store::open"), dir)
}

fn clip(uid: &str) -> NewAudio {
    NewAudio {
        uid: uid.into(),
        name: format!("clip {uid}"),
        description: Some("d".into()),
        kind: AudioKind::Clip,
        tts_text: None,
        tts_voice: None,
        version_token: Some("tok1".into()),
    }
}

fn tts(uid: &str, text: Option<&str>) -> NewAudio {
    NewAudio {
        kind: AudioKind::Tts,
        tts_text: text.map(Into::into),
        tts_voice: Some("en_f".into()),
        ..clip(uid)
    }
}

fn play(session: &str, event: &str) -> NewPlay {
    NewPlay {
        session_id: session.into(),
        origin: "rule".into(),
        rule_id: Some("r1".into()),
        step: Some(1),
        event_id: Some(event.into()),
        camera_id: None,
        speaker_id: 7,
        audio_uid: Some("a".into()),
        mode: "stored".into(),
    }
}

#[tokio::test]
async fn library_round_trip() {
    let (s, _d) = fresh_store().await;
    assert_eq!(s.count_talkdown_audio().await.unwrap(), 0);
    s.upsert_talkdown_pending(&clip("a")).await.unwrap();
    let a = s.get_talkdown_audio("a").await.unwrap().unwrap();
    assert_eq!(a.status, AudioStatus::Pending);
    assert_eq!(a.kind, AudioKind::Clip);
    assert!(!a.has_previous);
    assert_eq!(s.talkdown_audio_pcm("a").await.unwrap(), None);

    s.set_talkdown_ready("a", &[1, -2, 3, i16::MIN, i16::MAX])
        .await
        .unwrap();
    assert_eq!(
        s.talkdown_audio_pcm("a").await.unwrap().unwrap(),
        vec![1, -2, 3, i16::MIN, i16::MAX]
    );
    let a = s.get_talkdown_audio("a").await.unwrap().unwrap();
    assert_eq!(
        (a.status, a.duration_ms, a.has_previous),
        (AudioStatus::Ready, 0, false)
    );

    s.update_talkdown_meta("a", "renamed", None).await.unwrap();
    let a = s.get_talkdown_audio("a").await.unwrap().unwrap();
    assert_eq!((a.name.as_str(), a.description), ("renamed", None));

    s.set_talkdown_failed("a", "boom").await.unwrap();
    let a = s.get_talkdown_audio("a").await.unwrap().unwrap();
    assert_eq!(
        (a.status, a.error.as_deref()),
        (AudioStatus::Failed, Some("boom"))
    );
    assert_eq!(s.talkdown_audio_pcm("a").await.unwrap(), None);

    assert_eq!(s.count_talkdown_audio().await.unwrap(), 1);
    s.delete_talkdown_audio("a").await.unwrap();
    assert!(s.get_talkdown_audio("a").await.unwrap().is_none());
    for r in [
        s.delete_talkdown_audio("a").await,
        s.set_talkdown_ready("a", &[1]).await,
        s.set_talkdown_failed("a", "x").await,
        s.update_talkdown_meta("a", "n", None).await,
        s.revert_talkdown_audio("a").await,
    ] {
        assert!(matches!(r, Err(StoreError::NotFound(_))));
    }
}

#[tokio::test]
async fn list_has_no_samples_and_duration_from_samples() {
    let (s, _d) = fresh_store().await;
    s.upsert_talkdown_pending(&clip("a")).await.unwrap();
    s.set_talkdown_ready("a", &vec![5; 8000]).await.unwrap();
    let l = s.list_talkdown_audio().await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].duration_ms, 1000);
    // The metadata type has no sample field; also confirm the SQL never read it.
    let pcm_in_list =
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM talkdown_audio WHERE pcm IS NOT NULL")
            .fetch_one(s.pool())
            .await
            .unwrap();
    assert_eq!(pcm_in_list, 1);
}

#[tokio::test]
async fn ready_keeps_previous_and_revert_swaps() {
    let (s, _d) = fresh_store().await;
    s.upsert_talkdown_pending(&tts("t", Some("old text")))
        .await
        .unwrap();
    s.set_talkdown_ready("t", &[1, 1]).await.unwrap();
    assert!(
        !s.get_talkdown_audio("t")
            .await
            .unwrap()
            .unwrap()
            .has_previous
    );

    s.upsert_talkdown_pending(&tts("t", Some("new text")))
        .await
        .unwrap();
    // Pending keeps old samples unplayable but present.
    assert_eq!(s.talkdown_audio_pcm("t").await.unwrap(), None);
    s.set_talkdown_ready("t", &[2, 2, 2]).await.unwrap();
    let a = s.get_talkdown_audio("t").await.unwrap().unwrap();
    assert!(a.has_previous);
    assert_eq!(a.tts_text.as_deref(), Some("new text"));

    s.revert_talkdown_audio("t").await.unwrap();
    assert_eq!(
        s.talkdown_audio_pcm("t").await.unwrap().unwrap(),
        vec![1, 1]
    );
    let a = s.get_talkdown_audio("t").await.unwrap().unwrap();
    assert_eq!(a.tts_text.as_deref(), Some("old text"));
    assert!(a.has_previous);
    // Reverting again swaps forward.
    s.revert_talkdown_audio("t").await.unwrap();
    assert_eq!(
        s.talkdown_audio_pcm("t").await.unwrap().unwrap(),
        vec![2, 2, 2]
    );

    // Nothing to revert to.
    s.upsert_talkdown_pending(&clip("c")).await.unwrap();
    s.set_talkdown_ready("c", &[1]).await.unwrap();
    assert!(matches!(
        s.revert_talkdown_audio("c").await,
        Err(StoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn tts_without_text_is_refused() {
    let (s, _d) = fresh_store().await;
    assert!(s.upsert_talkdown_pending(&tts("t", None)).await.is_err());
    assert_eq!(s.count_talkdown_audio().await.unwrap(), 0);
}

#[tokio::test]
async fn plays_round_trip_and_prune() {
    let (s, _d) = fresh_store().await;
    let p1 = s.insert_talkdown_play(&play("s1", "e1")).await.unwrap();
    let p2 = s.insert_talkdown_play(&play("s2", "e2")).await.unwrap();
    s.finish_talkdown_play(p1, "played", None).await.unwrap();
    s.set_talkdown_play_outcome(p1, "cleared").await.unwrap();

    let all = s.list_talkdown_plays(&PlayFilter::default()).await.unwrap();
    assert_eq!(all.iter().map(|p| p.id).collect::<Vec<_>>(), vec![p2, p1]);
    let one = s
        .list_talkdown_plays(&PlayFilter {
            event_id: Some("e1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].result.as_deref(), Some("played"));
    assert_eq!(one[0].outcome.as_deref(), Some("cleared"));
    assert!(one[0].ended_at.is_some());
    assert!(s.finish_talkdown_play(999, "played", None).await.is_err());
    assert!(s.finish_talkdown_play(p2, "bogus", None).await.is_err());

    // Age p1 to 100 days; prune at 90 days keeps p2.
    sqlx::query("UPDATE talkdown_plays SET started_at = datetime('now', '-100 days') WHERE id = ?")
        .bind(p1)
        .execute(s.pool())
        .await
        .unwrap();
    let n = s
        .prune_talkdown_plays(Utc::now() - Duration::days(90))
        .await
        .unwrap();
    assert_eq!(n, 1);
    let left = s.list_talkdown_plays(&PlayFilter::default()).await.unwrap();
    assert_eq!(left.iter().map(|p| p.id).collect::<Vec<_>>(), vec![p2]);
}

#[tokio::test]
async fn core_settings_and_arming_expiry() {
    let (s, _d) = fresh_store().await;
    let c = s.talkdown_core().await.unwrap();
    assert!(c.auto_armed && !c.cloud_allowed && c.library_revision == 0);

    s.set_talkdown_core_cloud_allowed(true).await.unwrap();
    s.set_talkdown_core_library_revision(5).await.unwrap();
    let future = Utc::now() + Duration::hours(1);
    s.set_talkdown_core_arming(false, Some(future), Some("maintenance"))
        .await
        .unwrap();
    let c = s.talkdown_core().await.unwrap();
    assert!(!c.auto_armed && c.cloud_allowed && c.library_revision == 5);
    assert_eq!(c.auto_disarm_reason.as_deref(), Some("maintenance"));

    s.set_talkdown_core_arming(false, Some(Utc::now() - Duration::seconds(5)), None)
        .await
        .unwrap();
    assert!(
        s.talkdown_core().await.unwrap().auto_armed,
        "elapsed rearm_at reads as armed"
    );

    s.set_talkdown_core_arming(false, None, Some("indefinite"))
        .await
        .unwrap();
    assert!(!s.talkdown_core().await.unwrap().auto_armed);
}

#[tokio::test]
async fn speaker_arming() {
    let (s, _d) = fresh_store().await;
    let mut sp: SpeakerConfig =
        serde_json::from_value(serde_json::json!({ "name": "sp", "host": "192.0.2.10" })).unwrap();
    let mut tx = s.begin_tx().await.unwrap();
    s.create_speaker_tx(&mut tx, &mut sp).await.unwrap();
    Store::commit_tx(tx).await.unwrap();

    assert!(s.speaker_arming(sp.id).await.unwrap().unwrap().armed);
    s.set_speaker_arming(
        sp.id,
        false,
        Some(Utc::now() + Duration::hours(2)),
        Some("r"),
    )
    .await
    .unwrap();
    let a = s.speaker_arming(sp.id).await.unwrap().unwrap();
    assert!(!a.armed && a.reason.as_deref() == Some("r") && a.rearm_at.is_some());
    s.set_speaker_arming(sp.id, false, Some(Utc::now() - Duration::seconds(1)), None)
        .await
        .unwrap();
    assert!(s.speaker_arming(sp.id).await.unwrap().unwrap().armed);

    assert!(matches!(
        s.set_speaker_arming(999, false, None, None).await,
        Err(StoreError::NotFound(_))
    ));
    assert!(s.speaker_arming(999).await.unwrap().is_none());
    // The speaker config round-trips untouched by the new columns.
    assert_eq!(s.get_speaker(sp.id).await.unwrap().unwrap().name, "sp");
}
