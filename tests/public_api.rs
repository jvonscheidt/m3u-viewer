//! Public API integration tests.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use m3u_viewer::config::{Config, XtreamConfig};
use m3u_viewer::epg::EpgSource;
use m3u_viewer::loader::LoadEvent;
use m3u_viewer::playlist::Playlist;
use m3u_viewer::store::{Store, StoreError};
use m3u_viewer::xtream::Account;

#[test]
fn parses_playlist_through_public_accessors() -> Result<(), Box<dyn std::error::Error>> {
    let input = b"#EXTM3U\n#EXTINF:-1 tvg-id=\"news\" group-title=\"World\",News One\nhttp://u/1\n";
    let playlist = Playlist::from_reader(&input[..])?;
    let channel = &playlist.channels()[0];

    assert_eq!(channel.name(), "News One");
    assert_eq!(channel.url(), "http://u/1");
    assert_eq!(channel.tvg_id(), Some("news"));
    assert_eq!(
        channel.group().and_then(|id| playlist.group_name(id)),
        Some("World")
    );
    assert_eq!(playlist.skipped(), 0);
    Ok(())
}

#[test]
fn credential_debug_output_is_redacted() {
    let persisted = XtreamConfig::new(
        "http://example.com".to_owned(),
        "alice".to_owned(),
        "secret-one".to_owned(),
    );
    let account = Account::new(
        "http://example.com",
        "alice".to_owned(),
        "secret-two".to_owned(),
    );
    let config =
        Config::default().with_epg_url(Some("http://u:secret-three@example.com/epg".to_owned()));
    let epg_source = EpgSource::Url("http://host/epg?password=secret-four".to_owned());
    let load_event = LoadEvent::EpgUrl("http://host/epg?password=secret-five".to_owned());

    assert!(!format!("{persisted:?}").contains("secret-one"));
    assert!(!format!("{account:?}").contains("secret-two"));
    assert!(!format!("{config:?}").contains("secret-three"));
    assert!(!format!("{epg_source:?}").contains("secret-four"));
    assert!(!format!("{load_event:?}").contains("secret-five"));
}

#[test]
fn corrupt_store_is_reported_and_preserved() -> Result<(), Box<dyn std::error::Error>> {
    let unique = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "m3u-viewer-public-api-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&dir)?;
    let favorites = dir.join("favorites.json");
    fs::write(&favorites, "not json")?;

    let result = Store::load(dir.clone());
    assert!(matches!(result, Err(StoreError::Decode { .. })));
    assert_eq!(fs::read_to_string(favorites)?, "not json");

    fs::remove_dir_all(dir)?;
    Ok(())
}
