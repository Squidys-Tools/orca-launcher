//! Benchmarks for the ranking hot path.
//!
//! What a keystroke actually costs: one match per candidate, plus one sort of
//! the surviving candidates. The catalogue sizes here are the ones that matter
//! for the open performance problem in `docs/ARCHITECTURE.md` — a launcher
//! holding a few thousand candidates is the worst realistic case, and the fuzzy
//! tier is the only part of the matcher that is super-linear in practice.
//!
//! Run with `cargo bench -p orca-core --target x86_64-pc-windows-gnu`.
//! Criterion is a dev-dependency with `default-features = false`, so there is
//! no HTML report and no plotting toolchain to install.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use orca_core::providers::{Alias, CommandProvider, ResultProvider};
use orca_core::store::{LaunchRecord, MemoryUsageStore, UsageStore, UsageStoreExt};
use orca_core::{
    classify, match_score, rank_at, Frecency, LaunchTarget, RankingPolicy, ResultItem, Source,
    Timestamp,
};

const NOW: Timestamp = Timestamp::from_unix_seconds(1_700_000_000);
const DAY: i64 = 86_400;

/// The words used to build synthetic catalogues. Deliberately mixed-length and
/// mixed-shape so the matcher sees prefixes, word boundaries, fragments, and
/// non-matches rather than one easy case.
const WORDS: [&str; 16] = [
    "notepad",
    "calculator",
    "chrome",
    "terminal",
    "explorer",
    "vscode",
    "firefox",
    "steam",
    "discord",
    "spotify",
    "obsidian",
    "onenote",
    "outlook",
    "excel",
    "device-manager",
    "task-manager",
];

/// A long, wide candidate — the shape that makes the fuzzy tier expensive,
/// because a 24-character needle is an in-order subsequence of a great many
/// 64-character strings.
const LONG_CANDIDATE: &str =
    "the quick brown fox jumps over the lazy dog while compiling a launcher";

/// Builds a catalogue of `count` items with a realistic mix of titles,
/// subtitles, sources, priors, and histories.
fn catalogue(count: usize) -> Vec<ResultItem> {
    (0..count)
        .map(|index| {
            let word = WORDS[index % WORDS.len()];
            let source = Source::ALL[index % Source::COUNT];
            let mut item = ResultItem::new(
                format!("item:{index:05}"),
                // Every third title gets a numeric suffix, which is where the
                // word-boundary and fuzzy tiers actually get exercised.
                if index % 3 == 0 {
                    format!("{word} {index}.txt")
                } else {
                    format!("{word} {:02}", index % 100)
                },
                source,
                LaunchTarget::Command {
                    program: word.to_owned(),
                    args: Vec::new(),
                },
            )
            .with_score((index % 100) as f64 / 100.0);
            if index % 2 == 0 {
                item = item.with_subtitle(format!(r"C:\Users\chris\Documents\{word}"));
            }
            if index % 5 == 0 {
                item = item.with_frecency(Frecency::new(
                    (index % 17) as u32,
                    Some(NOW.saturating_add_secs(-(index as i64 % 30) * DAY)),
                ));
            }
            item
        })
        .collect()
}

/// A representative query: a prefix of one catalogue entry, a word-boundary hit
/// on another, a fragment on a third.
const TYPED_QUERY: &str = "note";

fn bench_match_score(c: &mut Criterion) {
    let mut group = c.benchmark_group("match_score");
    for candidate in ["notepad", "notepad.exe", "my downloads", LONG_CANDIDATE] {
        group.bench_with_input(BenchmarkId::new("exact", candidate), candidate, |b, c| {
            b.iter(|| std::hint::black_box(match_score("notepad", c)))
        });
        group.bench_with_input(BenchmarkId::new("prefix", candidate), candidate, |b, c| {
            b.iter(|| std::hint::black_box(match_score("not", c)))
        });
        group.bench_with_input(
            BenchmarkId::new("substring", candidate),
            candidate,
            |b, c| b.iter(|| std::hint::black_box(match_score("pad", c))),
        );
        group.bench_with_input(BenchmarkId::new("fuzzy", candidate), candidate, |b, c| {
            b.iter(|| std::hint::black_box(match_score("ntp", c)))
        });
        group.bench_with_input(
            BenchmarkId::new("no_match", candidate),
            candidate,
            |b, c| b.iter(|| std::hint::black_box(match_score("zzz", c))),
        );
    }
    group.finish();
}

fn bench_match_score_unicode(c: &mut Criterion) {
    let mut group = c.benchmark_group("match_score_unicode");
    // Case folding allocates and can change length, so the non-ASCII path is not
    // the ASCII path with a different constant.
    for (query, candidate) in [
        ("café", "Café Notes"),
        ("日本", "日本語のファイル"),
        ("straße", "STRASSE"),
        ("🎉", "party 🎉 time"),
    ] {
        group.bench_with_input(
            BenchmarkId::new(query, candidate),
            candidate,
            |b, candidate| b.iter(|| std::hint::black_box(match_score(query, candidate))),
        );
    }
    group.finish();
}

fn bench_classify(c: &mut Criterion) {
    let mut group = c.benchmark_group("classify");
    group.bench_function("browse", |b| {
        b.iter(|| std::hint::black_box(classify("", "Notepad.exe")))
    });
    group.bench_function("prefix", |b| {
        b.iter(|| std::hint::black_box(classify("not", "Notepad.exe")))
    });
    group.bench_function("no_match", |b| {
        b.iter(|| std::hint::black_box(classify("zzz", "Notepad.exe")))
    });
    group.finish();
}

fn bench_rank(c: &mut Criterion) {
    let mut group = c.benchmark_group("rank");
    for size in [100usize, 1_000, 5_000] {
        let items = catalogue(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("browse", size), &items, |b, items| {
            b.iter(|| std::hint::black_box(rank_at(RankingPolicy::DEFAULT, "", NOW, items)).len())
        });
        group.bench_with_input(BenchmarkId::new("typed", size), &items, |b, items| {
            b.iter(|| {
                std::hint::black_box(rank_at(RankingPolicy::DEFAULT, TYPED_QUERY, NOW, items)).len()
            })
        });
        // A long query against short titles is the worst case for the length
        // early-out, and a long query against long titles is the worst case
        // overall.
        group.bench_with_input(BenchmarkId::new("long_query", size), &items, |b, items| {
            b.iter(|| {
                std::hint::black_box(rank_at(
                    RankingPolicy::DEFAULT,
                    "device manager task",
                    NOW,
                    items,
                ))
                .len()
            })
        });
    }
    group.finish();
}

fn bench_frecency(c: &mut Criterion) {
    let mut group = c.benchmark_group("frecency");
    let half_life = orca_core::frecency::DEFAULT_HALF_LIFE;
    group.bench_function("never", |b| {
        b.iter(|| std::hint::black_box(Frecency::NEVER.score(NOW, half_life)))
    });
    group.bench_function("hot", |b| {
        let hot = Frecency::new(500, Some(NOW));
        b.iter(|| std::hint::black_box(hot.score(NOW, half_life)))
    });
    group.bench_function("stale", |b| {
        let stale = Frecency::new(1, Some(Timestamp::from_unix_seconds(0)));
        b.iter(|| std::hint::black_box(stale.score(NOW, half_life)))
    });
    group.finish();
}

fn bench_overlay(c: &mut Criterion) {
    let mut group = c.benchmark_group("store_overlay");
    let mut store = MemoryUsageStore::new();
    for index in 0..5_000usize {
        store
            .record_launch(LaunchRecord {
                item_id: &format!("item:{index:05}"),
                source: Source::ALL[index % Source::COUNT],
                title: "title",
                at: NOW.saturating_add_secs(-(index as i64 % 30) * DAY),
            })
            .expect("launch");
    }

    // Reading the whole history. This is the cost of stamping frecency onto a
    // catalogue, and it is the one non-ranking thing on the keystroke path.
    group.bench_function("frecency_map_5k", |b| {
        b.iter(|| std::hint::black_box(store.frecency_map().expect("map").len()))
    });

    group.bench_function("most_recent_5k", |b| {
        b.iter(|| std::hint::black_box(store.most_recent(20).expect("recent").len()))
    });
    group.finish();
}

fn bench_command_provider(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_provider");
    let provider = CommandProvider::new(
        (0..200).map(|index| Alias::new(format!("alias{index:03}"), format!("program{index}"))),
    );
    group.throughput(Throughput::Elements(200));
    group.bench_function("collect_200", |b| {
        b.iter(|| std::hint::black_box(provider.collect().expect("collect").len()))
    });
    group.finish();
}

criterion_group!(
    name = benches;
    // Criterion's default sample size is 100, which is a lot of wall clock for
    // a sub-microsecond benchmark. 40 is enough to see a regression and keeps a
    // full `cargo bench` run to a few seconds.
    config = Criterion::default()
        .sample_size(40)
        .measurement_time(Duration::from_secs(2))
        .warm_up_time(Duration::from_millis(200));
    targets = bench_match_score,
        bench_match_score_unicode,
        bench_classify,
        bench_rank,
        bench_frecency,
        bench_overlay,
        bench_command_provider,
);
criterion_main!(benches);
