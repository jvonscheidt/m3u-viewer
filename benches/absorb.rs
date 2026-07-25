//! Benchmark for the streaming-absorb path.
//!
//! [`App::on_load_event`] folds each arriving [`LoadEvent::Batch`] into the
//! running alphabetical order, so a large playlist is absorbed as a series
//! of batches rather than one big sort. This benchmark reproduces that
//! cold-load path — a fresh [`App`] fed the whole playlist in
//! [`BATCH_SIZE`]-channel batches — at a few sizes, so the per-batch merge
//! (and the scratch-buffer reuse that keeps its allocation traffic linear)
//! can be measured and kept honest as the list grows.

// `criterion_group!` expands to an undocumented public function; a bench
// harness is not a public API surface, so exempt this file from missing_docs.
#![allow(missing_docs)]

use std::hint::black_box;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use m3u_viewer::app::App;
use m3u_viewer::loader::LoadEvent;
use m3u_viewer::playlist::{Channel, PlaylistBuilder};

/// Channels per batch, mirroring the loader's own `BATCH_SIZE` so the
/// benchmark exercises the same number of merges a real load performs.
const BATCH_SIZE: usize = 4096;

/// Distinct group names spread across the channels.
const GROUP_COUNT: usize = 200;

/// A parsed playlist held as owned channels plus their group names, built
/// once and reused (via clone in untimed setup) across benchmark samples.
struct Fixture {
    channels: Vec<Channel>,
    groups: Vec<String>,
}

/// Names are deliberately scrambled relative to arrival order (a
/// multiplicative hash rendered as hex) so absorbing a batch has to
/// genuinely interleave it with the channels already placed, rather than
/// appending an already-sorted run.
fn scrambled_name(index: usize) -> String {
    let hashed = (index as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("Channel {hashed:016x}")
}

fn fixture(count: usize) -> Fixture {
    use std::fmt::Write as _;

    let mut text = String::from("#EXTM3U\n");
    for i in 0..count {
        let group = i % GROUP_COUNT;
        let name = scrambled_name(i);
        writeln!(
            text,
            "#EXTINF:-1 tvg-id=\"ch{i}.tv\" group-title=\"Group {group:03}\",{name}\nhttp://example.com/{i}"
        )
        .expect("writing to a String cannot fail");
    }

    let mut builder = PlaylistBuilder::new();
    for line in text.lines() {
        builder.push_line(line);
    }
    Fixture {
        channels: builder.drain_channels(),
        groups: builder.groups().to_vec(),
    }
}

/// Splits the fixture channels into owned per-batch vectors, ready to hand
/// to [`App::on_load_event`]. Runs in criterion's untimed setup so neither
/// the clone nor the split counts against the measured absorb.
fn batches(fixture: &Fixture) -> Vec<Vec<Channel>> {
    fixture
        .channels
        .chunks(BATCH_SIZE)
        .map(<[Channel]>::to_vec)
        .collect()
}

/// Feeds every batch (all group names announced with the first, exactly as
/// the loader does before the ids they reference are used) into a fresh
/// app, then finishes the load — the operation under measurement.
fn absorb_all(groups: &[String], batches: Vec<Vec<Channel>>) -> App {
    let mut app = App::new("bench".to_owned(), None);
    for (i, channels) in batches.into_iter().enumerate() {
        let new_groups = if i == 0 { groups.to_vec() } else { Vec::new() };
        app.on_load_event(LoadEvent::Batch {
            channels,
            new_groups,
            skipped: 0,
            percent: None,
        });
    }
    app.on_load_event(LoadEvent::Finished);
    app
}

fn bench_absorb(c: &mut Criterion) {
    let mut group = c.benchmark_group("absorb_cold_load");
    // Heavy per-sample work (a full multi-batch load); the default 100
    // samples would run for minutes at the larger sizes.
    group.sample_size(10);
    for &count in &[50_000_usize, 200_000] {
        let fixture = fixture(count);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(count),
            &fixture,
            |b, fixture| {
                b.iter_batched(
                    || batches(fixture),
                    |batches| black_box(absorb_all(&fixture.groups, batches)),
                    BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_absorb);
criterion_main!(benches);
